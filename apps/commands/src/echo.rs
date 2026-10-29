//! `echo`: print the argument string.

use feos_app::{App, Context};
use feos_console::println;

/// Prints its arguments back to the console.
pub struct Echo;

impl App for Echo {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn usage(&self) -> &'static str {
        "echo <text>"
    }

    fn description(&self) -> &'static str {
        "print text"
    }

    fn run(&self, context: &mut Context<'_>) {
        println!("{}", context.args);
    }
}
