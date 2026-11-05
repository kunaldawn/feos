//! `splash`: redraw the boot splash.

use feos_app::{App, Context, WindowContext, WindowFrame};
use feos_console::println;

/// Re-renders the boot splash, on the framebuffer from the shell and in a
/// window from the desktop.
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

    fn windowed(&self) -> bool {
        true
    }

    fn open_window(&self, _context: &WindowContext<'_, '_>) -> bool {
        true
    }

    fn frame_window(&self, context: &mut WindowContext<'_, '_>) -> WindowFrame {
        crate::ui::splash(context.canvas, context.platform.board.arch);
        WindowFrame::Continue
    }

    fn needs_display(&self) -> bool {
        true
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        match context.display.as_mut() {
            Some(canvas) => {
                crate::ui::splash(canvas, context.platform.board.arch);
                println!("redrew the boot splash");
            }
            None => {
                println!("no framebuffer; restart QEMU with -device ramfb");
                return 1;
            }
        }
        0
    }
}
