//! A minimal line-oriented serial shell.
//!
//! The shell owns line editing and dispatch, not the commands themselves. It
//! reads from the registered console and looks commands up in the app registry
//! (`feos-apps`), so adding a command never touches this crate.

#![no_std]

use feos_app::{Context, Platform};
use feos_console::{print, println, read_byte};
use feos_gfx::Canvas;

const MAX_LINE: usize = 128;
const PROMPT: &str = "feos> ";

/// Reads and executes commands forever.
pub fn run(mut display: Option<Canvas<'static>>, platform: &Platform) -> ! {
    let mut line = [0u8; MAX_LINE];
    let mut len = 0usize;

    print!("{PROMPT}");

    loop {
        let Some(byte) = read_byte() else {
            core::hint::spin_loop();
            continue;
        };

        match byte {
            b'\r' | b'\n' => {
                println!();
                if len > 0 {
                    let text = core::str::from_utf8(&line[..len]).unwrap_or("");
                    execute(text.trim(), &mut display, platform);
                }
                len = 0;
                print!("{PROMPT}");
            }
            0x7f | 0x08 => {
                if len > 0 {
                    len -= 1;
                    print!("\x08 \x08");
                }
            }
            0x03 => {
                // Ctrl-C: discard the current line.
                println!("^C");
                len = 0;
                print!("{PROMPT}");
            }
            b if (b.is_ascii_graphic() || b == b' ') && len < MAX_LINE => {
                line[len] = b;
                len += 1;
                print!("{}", b as char);
            }
            _ => {}
        }
    }
}

fn execute(line: &str, display: &mut Option<Canvas<'static>>, platform: &Platform) {
    let mut parts = line.splitn(2, ' ');
    let command = parts.next().unwrap_or("");
    let args = parts.next().unwrap_or("").trim();

    if command.is_empty() {
        return;
    }

    match feos_apps::find(command) {
        Some(app) => {
            let mut context = Context {
                args,
                display,
                viewport: None,
                suspend: None,
                suspend_area: None,
                platform,
            };
            app.run(&mut context);
        }
        None => println!("unknown command: {command} (type 'help')"),
    }
}
