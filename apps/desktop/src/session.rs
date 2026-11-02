//! Interactive desktop session driven by the serial console and pointer.
//!
//! Serial keys are the keyboard: arrow keys move the Start-menu highlight (or
//! the wallpaper selection), `Enter` launches the highlighted item, `Space`
//! opens the Start menu, and `Escape` closes it. A virtio tablet supplies the
//! pointer, so Start-menu items, taskbar buttons, desktop icons and window
//! controls behave like their Windows XP counterparts. `Ctrl-\` (or the Start
//! menu's "Log Off" button) returns to the shell.
//!
//! The console and pointer are polled, so this loop never blocks: the
//! scheduler keeps running background applications while the desktop redraws.

use feos_app::Platform;
use feos_console::println;
use feos_desktop::StartMenuCommand;
use feos_gfx::{Canvas, Point};
use feos_gui::{Event, Input, MouseButton};
use feos_hal::PointerEvent;
use feos_keys::SerialKeys;

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
const MINUTE_MS: u64 = 60_000;
/// How often supervised services and launched windows are polled.
const TICK_MS: u64 = 100;
/// Idle sleep between polls.
const FRAME_MS: u64 = 8;

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

    let config = super::load_config();

    let mut input = Input::new();
    let mut keys = SerialKeys::new();
    let Some(mut session) = super::SESSION.try_lock() else {
        println!("[desktop] a desktop session is already running");
        return SessionExit::Busy;
    };

    println!("[desktop] session started");
    println!(
        "[desktop] click Start, double-click desktop icons, drag windows; Ctrl-\\ returns to the shell"
    );
    super::prepare(&mut session, config, platform, true);

    let mut last_tick = platform.clock.uptime_ms();
    let mut last_minute = u64::MAX;
    let mut dirty = true;
    let mut pointer_position = platform.pointer.map(|pointer| pointer.position());
    if let Some((x, y)) = pointer_position {
        input.set_pointer(Point::new(x as i32, y as i32));
    }

    loop {
        let now = platform.clock.uptime_ms();
        if keys.poll(now, &mut input) {
            println!();
            println!("[desktop] log off - returning to the serial shell");
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

        let had_input = !input.is_empty();
        if had_input {
            dirty = true;
        }

        // The taskbar clock only shows hours and minutes; redrawing for every
        // second would repaint the whole screen for no visible change.
        let minute = now / MINUTE_MS;
        if minute != last_minute {
            last_minute = minute;
            dirty = true;
        }

        if now.wrapping_sub(last_tick) >= TICK_MS {
            if session.tick_session(platform.processes) > 0 {
                dirty = true;
            }
            last_tick = now;
        }

        if dirty {
            let response = if let Some(back) = back.as_mut() {
                session.render(back, &input, now)
            } else {
                let front = display.as_mut().expect("display checked at session start");
                session.render(front, &input, now)
            };
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
            }

            // A foreground app drew directly to the scanout buffer. Leave it
            // alone and compose a fresh desktop frame on the next pass. A
            // background launch also needs one more frame so its new window
            // becomes visible.
            if !ran_foreground {
                present(display, back);
            }
            // Some interactions mutate state after the frame has already been
            // painted (closing or minimising a window, for example). Compose
            // one more frame so that result becomes visible.
            dirty = ran_foreground || response.launch.is_some() || had_input;

            if response.command == StartMenuCommand::LogOff {
                println!("[desktop] log off - returning to the serial shell");
                return SessionExit::LogOff;
            }
        }

        input.clear_events();
        platform.clock.spin_ms(FRAME_MS);
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

/// Copies the composed frame to the scanout buffer.
fn present(display: &mut Option<Canvas<'static>>, back: &Option<Canvas<'static>>) {
    let (Some(front), Some(back)) = (display.as_mut(), back.as_ref()) else {
        return;
    };

    if front.stride() == back.stride() && front.pixels().len() == back.pixels().len() {
        front.pixels_mut().copy_from_slice(back.pixels());
    } else {
        front.blit(
            0,
            0,
            back.width() as u32,
            back.height() as u32,
            back.pixels(),
        );
    }
}
