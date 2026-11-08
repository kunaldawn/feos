//! Interactive desktop session driven by the serial console and pointer.
//!
//! Serial keys are the keyboard: arrow keys move the Start-menu highlight (or
//! the wallpaper selection), `Enter` launches the highlighted item, `Space`
//! opens the Start menu, and `Escape` closes it. A virtio tablet supplies the
//! pointer, so Start-menu items, taskbar buttons, desktop icons and window
//! controls work as they look. `Ctrl-\` (or the Start menu's "Log Off" button)
//! returns to the shell.
//!
//! The console and pointer are polled, so this loop never blocks: the
//! scheduler keeps running background applications while the desktop redraws.

use core::fmt::Write as _;

use feos_app::shell::Request;
use feos_app::{shell, Platform};
use feos_text::Text;
use feos_console::println;
use feos_desktop::{paint, CursorShape, StartMenuCommand};
use feos_gfx::{Canvas, Point, Rect};
use feos_gui::{Event, Input, Key, Modifiers, MouseButton, Painter};
use feos_hal::PointerEvent;
use feos_keys::{EvdevKeys, SerialKeys};

/// Why a desktop session returned to its caller.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SessionExit {
    /// The user chose "Log Off" or pressed `Ctrl-\`.
    LogOff,
    /// The board did not provide a framebuffer.
    NoDisplay,
    /// Another process already owns the desktop session.
    Busy,
}

/// Redraw the taskbar clock when the displayed minute changes.
/// How often to look for changes to the application folder.
const APPS_CHECK_MS: u64 = 1000;
const MINUTE_MS: u64 = 60_000;
/// How often supervised services and launched windows are polled.
const TICK_MS: u64 = 100;
/// Idle sleep between polls.
const FRAME_MS: u64 = 8;
/// Keyboard reports read per poll. A burst stays in the device's ring and is
/// read on the following frames, instead of overflowing the frame's queue.
const KEYBOARD_BUDGET: usize = 32;

/// Runs the desktop until the user logs off.
///
/// When no framebuffer is available the caller should fall back to the serial
/// shell; the session returns [`SessionExit::NoDisplay`] in that case.
///
/// `back` is an optional off-screen canvas. When present, frames are composed
/// there and copied to `display` in one pass, so the user never sees a
/// half-drawn desktop.
pub fn run_session(
    display: &mut Option<Canvas<'static>>,
    back: &mut Option<Canvas<'static>>,
    platform: &Platform,
) -> SessionExit {
    if display.is_none() {
        println!("[desktop] no framebuffer; staying in the serial shell");
        return SessionExit::NoDisplay;
    }

    let mut config = super::load_config();
    super::restore_settings(&mut config, platform);
    shell::publish_settings(config.settings_text().as_str());

    let mut input = Input::new();
    let mut keys = SerialKeys::new().with_ctrl_d_passthrough();
    let mut evdev = EvdevKeys::new();
    let Some(mut session) = super::SESSION.try_lock() else {
        println!("[desktop] a desktop session is already running");
        return SessionExit::Busy;
    };

    println!("[desktop] session started");
    println!(
        "[desktop] click Start, double-click desktop icons, drag windows; Ctrl-\\ returns to the shell"
    );
    super::prepare(&mut session, config, platform, true);
    // A size chosen in an earlier session, if this display can still do it.
    if let Some((width, height)) = config.resolution {
        let current = display.as_ref().map(|front| (front.width(), front.height()));
        if current != Some((usize::from(width), usize::from(height))) {
            change_mode(&mut session, display, back, platform, width, height);
        }
    }
    if let Some(front) = display.as_ref() {
        super::provide_buffers(&mut session, front.width(), front.height());
    }
    publish_modes();
    publish_settings(&session, display, 0);
    // A retained session keeps the last frame in `back` and leaves the pointer
    // out of it. Drawing straight to the scanout has nowhere to keep a frame.
    session.set_retained(back.is_some());
    shell::set_session_active(true);

    let tree = platform.filesystem.and_then(|fs| fs.tree());
    let mut seen_tree = tree.map_or(0, |tree| tree.generation());
    let mut apps = tree.map_or(super::AppsSignature::default(), super::apps_signature);
    let mut last_tree_check = 0u64;
    let mut last_tick = platform.clock.uptime_ms();
    let mut last_minute = u64::MAX;
    let mut last_animated_frame = 0u64;
    let mut animation_period = 0u32;
    let mut dirty = true;
    let mut pointer_position = platform.pointer.map(|pointer| pointer.position());
    if let Some((x, y)) = pointer_position {
        input.set_pointer(Point::new(x as i32, y as i32));
    }
    let mut cursor = SoftCursor::new();
    // The pointer the last frame asked for, and until when a launch shows the
    // working pointer instead of the arrow.
    let mut desktop_cursor = CursorShape::Arrow;
    let mut working_until = 0u64;
    let mut scratch = Input::new();
    // A new screen size waiting to be kept: the size to go back to, and when.
    let mut pending_mode: Option<PendingMode> = None;
    // Bumped when the published settings have to be read again though they read the same.
    let mut settings_revision = 0u32;
    let mut pending_mode_seconds = 0u32;

    // The longest one frame took, and a stuck desktop is what it's reported for: the
    // media boot test looks at it after reading large files from disk.
    let mut longest_frame_ms = 0;
    let mut frame_start = 0;
    loop {
        let begun = platform.clock.uptime_ms();
        if frame_start != 0 {
            longest_frame_ms = longest_frame_ms.max(begun.saturating_sub(frame_start));
        }
        frame_start = begun;
        let now = platform.clock.uptime_ms();
        if keys.poll(now, &mut input) {
            println!();
            println!("[desktop] longest frame {longest_frame_ms} ms");
            return log_off(&mut session, platform);
        }

        // A virtio keyboard (when QEMU exposes one) feeds the same GUI event
        // queue as the serial console, so windows work with either input.
        if let Some(keyboard) = platform.keyboard {
            let layout = session.config().keyboard_layout;
            let mut budget = KEYBOARD_BUDGET;
            // One report can decode to a key and its text, so stop while both
            // still fit.
            while budget > 0 && input.keyboard_capacity() >= 2 {
                let Some(event) = keyboard.poll() else {
                    break;
                };
                evdev.feed(event, &mut scratch);
                for decoded in scratch.events() {
                    input.push(layout.map_event(decoded));
                }
                scratch.clear_events();
                budget -= 1;
            }
        }

        // The serial console's exit bytes have a keyboard equivalent. Ctrl-\
        // always leaves. Ctrl-D is a terminal's end of input, so it only
        // leaves when no hosted app has the keyboard.
        let app_has_keyboard = session
            .windows()
            .focused_window()
            .is_some_and(|window| window.runtime.is_some());
        if wants_exit(&input, app_has_keyboard) {
            println!();
            return log_off(&mut session, platform);
        }

        // Drain the pointer device into this frame's GUI event queue. A
        // report carries a full position, so the session only has to turn
        // state changes into the events the toolkit understands.
        if let Some(pointer) = platform.pointer {
            let mut budget = 16;
            while budget > 0 {
                let Some(report) = pointer.poll() else {
                    break;
                };
                if let Some(last) = pointer_position.as_mut() {
                    if push_pointer(&mut input, report, last) {
                        dirty = true;
                    }
                }
                budget -= 1;
            }
        }

        // Requests apps posted during the last frame.
        while let Some(notification) = shell::take_notification() {
            session.notify(notification, now);
            dirty = true;
        }
        let mut ran_foreground = false;
        while let Some(request) = shell::take_request() {
            if let Request::Setting { key, value } = &request {
                if key.as_str() == "resolution" {
                    let previous = display.as_ref().map(|front| (front.width() as u16, front.height() as u16));
                    if let (Some(previous), Some((width, height)), None) =
                        (previous, parse_mode(value.as_str()), pending_mode)
                    {
                        if (width, height) != previous && change_mode(&mut session, display, back, platform, width, height) {
                            cursor.forget();
                            pending_mode = Some(PendingMode {
                                previous,
                                wanted: (width, height),
                                deadline_ms: now + MODE_CONFIRM_MS,
                            });
                        } else {
                            settings_revision += 1;
                            publish_settings(&session, display, settings_revision);
                        }
                    }
                }
            }
            ran_foreground |= super::apply_request(&mut session, request, platform, display);
            dirty = true;
        }

        if let Some(pending) = pending_mode {
            // The prompt goes on counting whether or not anyone's there to see it.
            let left = pending.deadline_ms.saturating_sub(now).div_ceil(1000) as u32;
            session.set_resolution_prompt(Some(left));
            let answer = session.take_resolution_answer();
            let expired = now >= pending.deadline_ms;
            if answer == Some(true) {
                pending_mode = None;
                session.set_resolution_prompt(None);
                let (width, height) = pending.wanted;
                let mut value = Text::<16>::new();
                let _ = write!(value, "{width}x{height}");
                session.apply_setting("resolution", value.as_str());
                println!("[desktop] display size kept: {width}x{height}");
                dirty = true;
            } else if answer == Some(false) || expired {
                pending_mode = None;
                session.set_resolution_prompt(None);
                let (width, height) = pending.previous;
                if change_mode(&mut session, display, back, platform, width, height) {
                    cursor.forget();
                    println!("[desktop] display size back to {width}x{height}");
                }
                settings_revision += 1;
                publish_settings(&session, display, settings_revision);
                dirty = true;
            } else if left != pending_mode_seconds {
                pending_mode_seconds = left;
                dirty = true;
            }
        }

        if !input.is_empty() {
            dirty = true;
        }

        // A package installed from a terminal shows up without logging off. The
        // generation check is one load, so an idle system doesn't touch `/apps`.
        if let Some(tree) = tree {
            if now.saturating_sub(last_tree_check) >= APPS_CHECK_MS
                && tree.generation() != seen_tree
            {
                last_tree_check = now;
                seen_tree = tree.generation();
                let signature = super::apps_signature(tree);
                if signature != apps {
                    apps = signature;
                    let report = super::scan_all(&mut session, platform, true);
                    println!(
                        "[desktop] {} application(s) after a change to the application folder",
                        report.installed
                    );
                    dirty = true;
                }
            }
        }

        // The taskbar clock only shows hours and minutes; redrawing for every
        // second would repaint the whole screen for no visible change.
        let minute = platform
            .clock
            .unix_time()
            .map_or(now / MINUTE_MS, |seconds| seconds * 1000 / MINUTE_MS);
        if minute != last_minute {
            last_minute = minute;
            dirty = true;
        }

        if now.wrapping_sub(last_tick) >= TICK_MS {
            if session.tick_session_at(platform.processes, platform.clock.uptime_ms()) > 0 {
                dirty = true;
            }
            if session.expire_toasts(now) {
                dirty = true;
            }
            session.tick_background(platform, now);
            session.publish_windows();
            last_tick = now;
        }

        if ran_foreground {
            session.invalidate();
            cursor.forget();
        }

        if dirty {
            let response = if let Some(back) = back.as_mut() {
                session.render_with_platform(back, &input, now, Some(platform))
            } else {
                let front = display.as_mut().expect("display checked at session start");
                session.render_with_platform(front, &input, now, Some(platform))
            };
            if session.take_settings_changed() {
                super::save_settings(session.config(), platform);
                publish_settings(&session, display, settings_revision);
            }

            // A foreground app paints directly into the scanout and only the
            // work area. Present the freshly composed desktop first so the
            // taskbar (including the new task button) is already visible
            // underneath the app while it runs.
            let may_run_foreground = response.launch.is_some()
                || response.run.is_some()
                || response.command != StartMenuCommand::None;
            let shown_at = platform.clock.uptime_ms();
            if response.launch.is_some() || response.run.is_some() {
                working_until = shown_at + LAUNCH_FEEDBACK_MS;
            }
            desktop_cursor = response.cursor;
            let shape = pointer_shape(desktop_cursor, shown_at, working_until);
            if may_run_foreground {
                present(display, back, response.damage, input.pointer(), shape, shown_at, &mut cursor);
            }

            let ran_foreground = super::apply_response(&mut session, &response, platform, display);

            // The foreground app may have consumed pointer reports while it
            // owned the screen. Resynchronise the cursor before composing the
            // desktop again so it does not jump back to its pre-launch spot.
            if ran_foreground {
                if let Some(pointer) = platform.pointer {
                    let (x, y) = pointer.position();
                    pointer_position = Some((x, y));
                    input.set_pointer(Point::new(x as i32, y as i32));
                }
                session.invalidate();
                cursor.forget();
            }

            // A foreground app drew directly to the scanout buffer. Leave it
            // alone and compose a fresh desktop frame on the next pass.
            if !ran_foreground {
                present(display, back, response.damage, input.pointer(), shape, shown_at, &mut cursor);
            }
            // Some interactions change state after the frame was painted
            // (closing a window, say), and a launch opens one. Only those need
            // a second frame; a key typed into an open app does not.
            dirty = ran_foreground
                || response.launch.is_some()
                || response.run.is_some()
                || response.command != StartMenuCommand::None
                || response.state_changed;
            animation_period = response.animation_period_ms;

            if response.command == StartMenuCommand::LogOff {
                return log_off(&mut session, platform);
            }
        }

        // Windowed apps can animate (the media player, live task-manager
        // graphs). This check must run even when the desktop is otherwise
        // idle: otherwise no frame is rendered, no `DesktopResponse` is
        // produced, and an open task manager's graph never advances.
        if animation_period > 0 && now.wrapping_sub(last_animated_frame) >= animation_period as u64
        {
            last_animated_frame = now;
            dirty = true;
        }
        // An app can start animating on its own: a terminal's background job has
        // just printed something, say. No frame has reported that yet.
        if animation_period == 0 {
            animation_period = session.animation_period();
        }

        // An animated pointer moves on by itself, and the launch feedback ends
        // by itself; neither needs the desktop composed again.
        let now = platform.clock.uptime_ms();
        let shape = pointer_shape(desktop_cursor, now, working_until);
        if cursor.due(now) || (cursor.drawn.is_some() && shape != cursor.shape) {
            present(display, back, None, input.pointer(), shape, now, &mut cursor);
        }

        input.clear_events();
        platform.clock.idle_ms(FRAME_MS);
    }
}

/// Ends the session: every open window closes, as on Windows, so the apps
/// behind them stop with it instead of running on with nothing to show them.
/// Session services keep running; they belong to the machine, not the
/// windows.
fn log_off(session: &mut super::Session, platform: &Platform) -> SessionExit {
    println!("[desktop] log off - returning to the serial shell");
    let closed = session.close_all_windows(platform.processes);
    if closed > 0 {
        println!("[desktop] closed {closed} window(s)");
    }
    shell::set_session_active(false);
    SessionExit::LogOff
}

/// Whether this frame's keys ask to leave the desktop.
fn wants_exit(input: &Input, app_has_keyboard: bool) -> bool {
    input.keys().any(|(key, modifiers)| {
        modifiers.contains(Modifiers::CTRL)
            && match key {
                Key::Character('\\') => true,
                Key::Character('d' | 'D') => !app_has_keyboard,
                _ => false,
            }
    })
}

/// How long a new screen size has to be kept before it goes back by itself.
const MODE_CONFIRM_MS: u64 = 15_000;

/// A new screen size that hasn't been kept yet.
#[derive(Clone, Copy)]
struct PendingMode {
    previous: (u16, u16),
    wanted: (u16, u16),
    deadline_ms: u64,
}

fn parse_mode(text: &str) -> Option<(u16, u16)> {
    let (width, height) = text.split_once('x')?;
    Some((width.parse().ok()?, height.parse().ok()?))
}

/// Tells the Settings app what this display can be set to.
fn publish_modes() {
    let mut text = Text::<128>::new();
    for (width, height) in feos_hal::display::modes() {
        let _ = write!(text, "{width}x{height} ");
    }
    shell::publish_display_modes(text.as_str());
}

/// Tells the Settings app what's set: the size that's on the screen, whatever was saved.
fn publish_settings(session: &super::Session, display: &Option<Canvas<'static>>, revision: u32) {
    let mut text = Text::<256>::new();
    let mut config = *session.config();
    if let Some(front) = display.as_ref() {
        config.resolution = Some((front.width() as u16, front.height() as u16));
    }
    let _ = writeln!(text, "{}revision: {revision}", config.settings_text().as_str());
    shell::publish_settings(text.as_str());
}

/// Changes the screen size: the mode, the two canvases, the pointer's range, and what the
/// session thinks. `false`, with nothing changed, when the display can't.
fn change_mode(
    session: &mut super::Session,
    display: &mut Option<Canvas<'static>>,
    back: &mut Option<Canvas<'static>>,
    platform: &Platform,
    width: u16,
    height: u16,
) -> bool {
    let Some(buffers) = feos_hal::display::set_mode(u32::from(width), u32::from(height)) else {
        println!("[desktop] can't set the display to {width}x{height}");
        return false;
    };
    let (columns, rows) = (usize::from(width), usize::from(height));
    *display = Canvas::new(buffers.front, columns, rows, columns);
    *back = Canvas::new(buffers.back, columns, rows, columns);
    // Nothing's looking at the old pixels now.
    feos_hal::display::release_old_mode();
    if let Some(pointer) = platform.pointer {
        pointer.set_screen(width, height);
    }
    session.screen_changed(columns, rows);
    super::replace_buffers(session, columns, rows);
    feos_hal::display::flush(0, 0, u32::from(width), u32::from(height));
    println!("[desktop] display now {width}x{height}");
    true
}

/// The pointer, drawn on the scanout over the composed frame.
///
/// The composed frame stays free of cursor pixels, so moving the pointer only
/// has to restore the old cursor's rectangle from it and draw the new one. The
/// back buffer is the save-under. The rectangle is the cursor's whole image
/// placed by its hot spot, which for the XP cursors starts up to 16 pixels
/// above and left of the pointer, so what's restored is always all that was
/// drawn.
struct SoftCursor {
    drawn: Option<Rect>,
    position: Point,
    shape: CursorShape,
    /// When the shape appeared, which is when an animation starts.
    since: u64,
    /// When an animated shape next shows another image.
    next_frame_at: Option<u64>,
}

/// What a cursor update drew over: where the old cursor was and the new one is.
#[derive(Clone, Copy, Default)]
struct CursorDamage {
    old: Option<Rect>,
    new: Option<Rect>,
}

impl SoftCursor {
    const fn new() -> Self {
        Self {
            drawn: None,
            position: Point::new(0, 0),
            shape: CursorShape::Arrow,
            since: 0,
            next_frame_at: None,
        }
    }

    /// Forgets the drawn cursor, because something else repainted the scanout.
    fn forget(&mut self) {
        self.drawn = None;
    }

    /// Whether an animated cursor is due to show its next image.
    fn due(&self, now: u64) -> bool {
        self.drawn.is_some() && self.next_frame_at.is_some_and(|at| now >= at)
    }

    /// Restores what the cursor covered, if it needs to move, change or was
    /// erased, and draws it.
    fn update(
        &mut self,
        front: &mut Canvas<'static>,
        back: &Canvas<'static>,
        position: Point,
        shape: CursorShape,
        damage: Option<Rect>,
        now: u64,
    ) -> CursorDamage {
        let overwritten = match (self.drawn, damage) {
            (Some(drawn), Some(damage)) => drawn.intersect(damage).is_some(),
            (None, _) => true,
            _ => false,
        };
        if shape != self.shape {
            self.since = now;
        }
        if !overwritten && position == self.position && shape == self.shape && !self.due(now) {
            return CursorDamage::default();
        }
        let old = self.drawn;
        if let Some(drawn) = old {
            front.copy_rect_from(back, drawn);
        }
        let elapsed = now.saturating_sub(self.since);
        paint::draw_cursor_shape_at(&mut Painter::new(front), position.x, position.y, shape, elapsed);
        let new = paint::cursor_bounds(position.x, position.y, shape);
        self.drawn = Some(new);
        self.next_frame_at = paint::cursor_frame_left(shape, elapsed).map(|left| now + left);
        self.position = position;
        self.shape = shape;
        CursorDamage {
            old,
            new: Some(new),
        }
    }
}

/// How long the working-in-background pointer shows after a launch, as XP's
/// launch feedback does. Opening a window happens inside one frame here, so
/// there's nothing finer to wait on.
const LAUNCH_FEEDBACK_MS: u64 = 1_500;

/// The pointer to show: the desktop's, unless a launch has just happened and
/// the desktop wants the plain arrow.
fn pointer_shape(desktop: CursorShape, now: u64, working_until: u64) -> CursorShape {
    if desktop == CursorShape::Arrow && now < working_until {
        CursorShape::Working
    } else {
        desktop
    }
}

/// Converts one driver report into toolkit events.
///
/// The pointer position persists in [`Input`]; a movement event is only
/// queued when the cursor actually changed, so a button press at the same
/// position does not produce a redundant move.
fn push_pointer(input: &mut Input, report: PointerEvent, last: &mut (u16, u16)) -> bool {
    let mut changed = false;
    let position = Point::new(report.x as i32, report.y as i32);

    if (report.x, report.y) != *last {
        *last = (report.x, report.y);
        input.push(Event::PointerMoved(position));
        changed = true;
    }

    for (button, state) in [
        (MouseButton::Left, report.left),
        (MouseButton::Right, report.right),
        (MouseButton::Middle, report.middle),
    ] {
        let Some(down) = state else {
            continue;
        };
        let event = if down {
            Event::PointerDown {
                pos: position,
                button,
            }
        } else {
            Event::PointerUp {
                pos: position,
                button,
            }
        };
        input.push(event);
        changed = true;
    }

    if report.wheel != 0 {
        input.push(Event::PointerWheel(report.wheel));
        changed = true;
    }

    changed
}

/// Copies what the frame changed to the scanout buffer, then the pointer.
fn present(
    display: &mut Option<Canvas<'static>>,
    back: &Option<Canvas<'static>>,
    damage: Option<Rect>,
    pointer: Point,
    shape: CursorShape,
    now: u64,
    cursor: &mut SoftCursor,
) {
    let (Some(front), Some(back)) = (display.as_mut(), back.as_ref()) else {
        return;
    };

    if let Some(damage) = damage {
        front.copy_rect_from(back, damage);
    }
    let drawn = cursor.update(front, back, pointer, shape, damage, now);
    report_damage(damage, drawn);
}

/// Tells a display that needs telling what changed: the frame's damage, and
/// where the cursor was and is, which it drew over outside the damage.
fn report_damage(damage: Option<Rect>, cursor: CursorDamage) {
    if !feos_hal::display::needs_refresh() {
        return;
    }
    for rect in [damage, cursor.old, cursor.new].into_iter().flatten() {
        // A cursor at the top or left edge starts off the screen.
        let (x, y) = (rect.x.max(0), rect.y.max(0));
        let width = (rect.x + rect.width as i32 - x).max(0) as u32;
        let height = (rect.y + rect.height as i32 - y).max(0) as u32;
        if width > 0 && height > 0 {
            feos_hal::display::flush(x as u32, y as u32, width, height);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(key: Key, modifiers: Modifiers) -> Input {
        let mut input = Input::new();
        input.push(Event::KeyDown(key, modifiers));
        input
    }

    #[test]
    fn ctrl_backslash_always_leaves() {
        let input = keys(Key::Character('\\'), Modifiers::CTRL);
        assert!(wants_exit(&input, false));
        assert!(wants_exit(&input, true));
    }

    #[test]
    fn ctrl_d_belongs_to_a_focused_app() {
        let input = keys(Key::Character('d'), Modifiers::CTRL);
        assert!(wants_exit(&input, false));
        assert!(!wants_exit(&input, true));
    }

    #[test]
    fn plain_keys_do_not_leave() {
        assert!(!wants_exit(&keys(Key::Character('d'), Modifiers::NONE), false));
    }

    fn solid(pixels: &'static mut [u32], width: usize, height: usize, color: u32) -> Canvas<'static> {
        pixels.fill(color);
        Canvas::new(pixels, width, height, width).unwrap()
    }

    fn leaked(width: usize, height: usize) -> &'static mut [u32] {
        std::vec![0u32; width * height].leak()
    }

    #[test]
    fn moving_the_soft_cursor_restores_what_it_covered() {
        let mut front = solid(leaked(64, 64), 64, 64, 0x0011_2233);
        let back = solid(leaked(64, 64), 64, 64, 0x0011_2233);
        let mut cursor = SoftCursor::new();
        cursor.update(&mut front, &back, Point::new(20, 20), CursorShape::Arrow, None, 0);
        assert_ne!(front.pixels(), back.pixels());

        cursor.update(&mut front, &back, Point::new(52, 52), CursorShape::Arrow, None, 0);
        // Nothing is left of the old cursor.
        for y in 0..40usize {
            for x in 0..40usize {
                assert_eq!(front.pixels()[y * 64 + x], 0x0011_2233, "{x},{y}");
            }
        }
        assert_ne!(front.pixels()[54 * 64 + 53], 0x0011_2233);
    }

    #[test]
    fn an_idle_cursor_costs_nothing_until_damage_reaches_it() {
        let mut front = solid(leaked(64, 64), 64, 64, 1);
        let back = solid(leaked(64, 64), 64, 64, 1);
        let mut cursor = SoftCursor::new();
        cursor.update(&mut front, &back, Point::new(4, 4), CursorShape::Arrow, None, 0);
        // A marker where the cursor is not; a redraw would not touch it either,
        // so check the cursor survives damage that erases its pixels.
        let far = Some(Rect::new(40, 40, 8, 8));
        let tip = front.pixels()[4 * 64 + 4];
        cursor.update(&mut front, &back, Point::new(4, 4), CursorShape::Arrow, far, 0);
        assert_eq!(front.pixels()[4 * 64 + 4], tip);

        front.copy_rect_from(&back, Rect::new(0, 0, 32, 32));
        let near = Some(Rect::new(0, 0, 32, 32));
        cursor.update(&mut front, &back, Point::new(4, 4), CursorShape::Arrow, near, 0);
        assert_eq!(front.pixels()[4 * 64 + 4], tip);
    }

    #[test]
    fn every_shape_moves_without_leaving_a_trail() {
        for shape in CursorShape::ALL {
            let mut front = solid(leaked(96, 96), 96, 96, 0x0011_2233);
            let back = solid(leaked(96, 96), 96, 96, 0x0011_2233);
            let mut cursor = SoftCursor::new();
            for (step, point) in [(2, 2), (40, 40), (90, 5), (5, 90), (60, 60)].into_iter().enumerate() {
                let drawn = cursor.update(&mut front, &back, Point::new(point.0, point.1), shape, None, step as u64 * 250);
                let new = drawn.new.unwrap();
                for (index, &pixel) in front.pixels().iter().enumerate() {
                    let at = Point::new((index % 96) as i32, (index / 96) as i32);
                    if pixel != 0x0011_2233 {
                        assert!(new.contains(at), "{shape:?} left {at:?} outside {new:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn an_animated_cursor_is_due_again_and_a_still_one_is_not() {
        let mut front = solid(leaked(64, 64), 64, 64, 1);
        let back = solid(leaked(64, 64), 64, 64, 1);
        let mut cursor = SoftCursor::new();
        cursor.update(&mut front, &back, Point::new(30, 30), CursorShape::Arrow, None, 0);
        assert!(!cursor.due(10_000));
        cursor.update(&mut front, &back, Point::new(30, 30), CursorShape::Busy, None, 100);
        assert!(!cursor.due(100));
        assert!(cursor.due(100 + 2_000));
        let first = front.pixels().to_vec();
        let drawn = cursor.update(&mut front, &back, Point::new(30, 30), CursorShape::Busy, None, 100 + 2_000);
        assert!(drawn.new.is_some());
        assert_ne!(front.pixels(), &first[..]);
    }

    #[test]
    fn a_launch_shows_the_working_pointer_for_a_while() {
        assert_eq!(pointer_shape(CursorShape::Arrow, 10, 500), CursorShape::Working);
        assert_eq!(pointer_shape(CursorShape::SizeVertical, 10, 500), CursorShape::SizeVertical);
        assert_eq!(pointer_shape(CursorShape::Arrow, 500, 500), CursorShape::Arrow);
    }
}
