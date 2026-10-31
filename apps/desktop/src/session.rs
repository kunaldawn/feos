//! Interactive desktop session driven by the serial console.
//!
//! The QEMU board has no pointer or keyboard device yet, so the serial console
//! acts as the keyboard. Arrow keys move the Start-menu highlight (or the
//! wallpaper selection), `Enter` launches the highlighted item, `Space` opens
//! the Start menu, and `Escape` closes it. `Ctrl-\` (or the Start menu's
//! "Log Off" button) returns to the shell.
//!
//! The console is polled, so this loop never blocks: the scheduler keeps
//! running background applications while the desktop redraws.

use feos_app::Platform;
use feos_console::println;
use feos_desktop::StartMenuCommand;
use feos_gfx::Canvas;
use feos_gui::Input;
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
        "[desktop] arrows select, Enter launches, Space opens Start, Esc closes, Ctrl-\\ returns to the shell"
    );
    super::prepare(&mut session, config, platform, true);

    let mut last_tick = platform.clock.uptime_ms();
    let mut last_minute = u64::MAX;
    let mut dirty = true;

    loop {
        let now = platform.clock.uptime_ms();
        if keys.poll(now, &mut input) {
            println!();
            println!("[desktop] log off - returning to the serial shell");
            return SessionExit::LogOff;
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
