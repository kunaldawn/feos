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

use feos_app::{shell, Platform};
use feos_console::println;
use feos_desktop::{paint, CursorShape, DesktopResponse, StartMenuCommand};
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
    if let Some(front) = display.as_ref() {
        super::provide_buffers(&mut session, front.width(), front.height());
    }
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
    let mut scratch = Input::new();

    loop {
        let now = platform.clock.uptime_ms();
        if keys.poll(now, &mut input) {
            println!();
            println!("[desktop] log off - returning to the serial shell");
            shell::set_session_active(false);
            return SessionExit::LogOff;
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
            println!("[desktop] log off - returning to the serial shell");
            shell::set_session_active(false);
            return SessionExit::LogOff;
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
            ran_foreground |= super::apply_request(&mut session, request, platform, display);
            dirty = true;
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
            }

            // A foreground app paints directly into the scanout and only the
            // work area. Present the freshly composed desktop first so the
            // taskbar (including the new task button) is already visible
            // underneath the app while it runs.
            let may_run_foreground = response.launch.is_some()
                || response.run.is_some()
                || response.command != StartMenuCommand::None;
            if may_run_foreground {
                present(display, back, &response, input.pointer(), &mut cursor);
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
                present(display, back, &response, input.pointer(), &mut cursor);
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
                println!("[desktop] log off - returning to the serial shell");
                shell::set_session_active(false);
                return SessionExit::LogOff;
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

        input.clear_events();
        platform.clock.idle_ms(FRAME_MS);
    }
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

/// The pointer, drawn on the scanout over the composed frame.
///
/// The composed frame stays free of cursor pixels, so moving the pointer only
/// has to restore the old cursor's rectangle from it and draw the new one. The
/// back buffer is the save-under.
struct SoftCursor {
    drawn: Option<Rect>,
    position: Point,
    shape: CursorShape,
}

impl SoftCursor {
    const fn new() -> Self {
        Self {
            drawn: None,
            position: Point::new(0, 0),
            shape: CursorShape::Arrow,
        }
    }

    /// Forgets the drawn cursor, because something else repainted the scanout.
    fn forget(&mut self) {
        self.drawn = None;
    }

    /// Restores what the cursor covered, if it needs to move or was erased, and
    /// draws it.
    fn update(
        &mut self,
        front: &mut Canvas<'static>,
        back: &Canvas<'static>,
        position: Point,
        shape: CursorShape,
        damage: Option<Rect>,
    ) {
        let overwritten = match (self.drawn, damage) {
            (Some(drawn), Some(damage)) => drawn.intersect(damage).is_some(),
            (None, _) => true,
            _ => false,
        };
        if !overwritten && position == self.position && shape == self.shape {
            return;
        }
        if let Some(drawn) = self.drawn {
            front.copy_rect_from(back, drawn);
        }
        paint::draw_cursor_shape(&mut Painter::new(front), position.x, position.y, shape);
        self.drawn = Some(paint::cursor_bounds(position.x, position.y, shape));
        self.position = position;
        self.shape = shape;
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
    response: &DesktopResponse,
    pointer: Point,
    cursor: &mut SoftCursor,
) {
    let (Some(front), Some(back)) = (display.as_mut(), back.as_ref()) else {
        return;
    };

    if let Some(damage) = response.damage {
        front.copy_rect_from(back, damage);
    }
    cursor.update(front, back, pointer, response.cursor, response.damage);
    report_damage(response.damage, pointer);
}

/// Tells a display that needs telling what changed: the frame's damage, and where
/// the pointer is and was, which the soft cursor drew over outside the damage.
fn report_damage(damage: Option<Rect>, pointer: Point) {
    use core::sync::atomic::{AtomicU64, Ordering};
    static LAST_POINTER: AtomicU64 = AtomicU64::new(0);
    if !feos_hal::display::needs_refresh() {
        return;
    }
    if let Some(damage) = damage {
        feos_hal::display::flush(
            damage.x.max(0) as u32,
            damage.y.max(0) as u32,
            damage.width,
            damage.height,
        );
    }
    let packed = (pointer.x.max(0) as u64) << 32 | pointer.y.max(0) as u64;
    let last = LAST_POINTER.swap(packed, Ordering::AcqRel);
    for spot in [last, packed] {
        let (x, y) = ((spot >> 32) as u32, spot as u32);
        feos_hal::display::flush(x, y, CURSOR_REACH, CURSOR_REACH);
    }
}

/// A square that holds the soft cursor, from its hot spot.
const CURSOR_REACH: u32 = 32;

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
        cursor.update(&mut front, &back, Point::new(4, 4), CursorShape::Arrow, None);
        assert_ne!(front.pixels(), back.pixels());

        cursor.update(&mut front, &back, Point::new(40, 40), CursorShape::Arrow, None);
        // Nothing is left of the old cursor.
        for y in 0..30usize {
            for x in 0..30usize {
                assert_eq!(front.pixels()[y * 64 + x], 0x0011_2233, "{x},{y}");
            }
        }
        assert_ne!(front.pixels()[42 * 64 + 41], 0x0011_2233);
    }

    #[test]
    fn an_idle_cursor_costs_nothing_until_damage_reaches_it() {
        let mut front = solid(leaked(64, 64), 64, 64, 1);
        let back = solid(leaked(64, 64), 64, 64, 1);
        let mut cursor = SoftCursor::new();
        cursor.update(&mut front, &back, Point::new(4, 4), CursorShape::Arrow, None);
        // A marker where the cursor is not; a redraw would not touch it either,
        // so check the cursor survives damage that erases its pixels.
        let far = Some(Rect::new(40, 40, 8, 8));
        let tip = front.pixels()[4 * 64 + 4];
        cursor.update(&mut front, &back, Point::new(4, 4), CursorShape::Arrow, far);
        assert_eq!(front.pixels()[4 * 64 + 4], tip);

        front.copy_rect_from(&back, Rect::new(0, 0, 32, 32));
        let near = Some(Rect::new(0, 0, 32, 32));
        cursor.update(&mut front, &back, Point::new(4, 4), CursorShape::Arrow, near);
        assert_eq!(front.pixels()[4 * 64 + 4], tip);
    }
}
