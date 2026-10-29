//! `clear`: clear the terminal.

use feos_app::{App, Context};
use feos_console::print;

/// Emits the ANSI clear-screen sequence.
pub struct Clear;

impl App for Clear {
    fn name(&self) -> &'static str {
        "clear"
    }

    fn usage(&self) -> &'static str {
        "clear"
    }

    fn description(&self) -> &'static str {
        "clear the terminal"
    }

    fn run(&self, _context: &mut Context<'_>) {
        print!("\x1b[2J\x1b[H");
    }
}
