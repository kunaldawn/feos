//! `gfx`: draw the graphics test card.

use feos_app::{App, Context};
use feos_console::println;

/// Renders the `feos-gfx` test card.
pub struct Gfx;

impl App for Gfx {
    fn name(&self) -> &'static str {
        "gfx"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["demo"]
    }

    fn usage(&self) -> &'static str {
        "gfx"
    }

    fn description(&self) -> &'static str {
        "draw the graphics test card"
    }

    fn needs_display(&self) -> bool {
        true
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        match context.display.as_mut() {
            Some(canvas) => {
                crate::ui::demo(canvas);
                println!(
                    "drew the test card on a {}x{} ramfb framebuffer",
                    canvas.width(),
                    canvas.height()
                );
            }
            None => {
                println!("no framebuffer; restart QEMU with -device ramfb");
                return 1;
            }
        }
        0
    }
}
