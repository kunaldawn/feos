//! `splash`: redraw the boot splash.

use feos_app::{App, Context};
use feos_console::println;

/// Re-renders the boot splash.
pub struct Splash;

impl App for Splash {
    fn name(&self) -> &'static str {
        "splash"
    }

    fn usage(&self) -> &'static str {
        "splash"
    }

    fn description(&self) -> &'static str {
        "redraw the boot splash"
    }

    fn run(&self, context: &mut Context<'_>) {
        match context.display.as_mut() {
            Some(canvas) => {
                crate::ui::splash(canvas);
                println!("redrew the boot splash");
            }
            None => println!("no framebuffer; restart QEMU with -device ramfb"),
        }
    }
}
