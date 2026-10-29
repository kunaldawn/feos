//! `help`: list the registered apps.

use feos_app::{App, Context};
use feos_console::println;

/// Prints the app registry.
pub struct Help;

impl App for Help {
    fn name(&self) -> &'static str {
        "help"
    }

    fn usage(&self) -> &'static str {
        "help"
    }

    fn description(&self) -> &'static str {
        "show this help"
    }

    fn run(&self, _context: &mut Context<'_>) {
        println!("apps:");
        for app in crate::REGISTRY {
            println!("  {:<20} {}", app.usage(), app.description());
        }
    }
}
