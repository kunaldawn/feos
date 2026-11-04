//! `info`: kernel, CPU, board and display information.

use feos_app::{App, Context, VERSION};
use feos_console::println;

/// Prints a summary of the running system.
pub struct Info;

impl App for Info {
    fn name(&self) -> &'static str {
        "info"
    }

    fn usage(&self) -> &'static str {
        "info"
    }

    fn description(&self) -> &'static str {
        "kernel, CPU, board and display information"
    }

    fn run(&self, context: &mut Context<'_>) {
        let platform = context.platform;
        let (kernel_start, kernel_end) = platform.board.kernel_range;
        let (bss_start, bss_end) = platform.board.bss_range;

        println!("feos {VERSION}");
        println!("  board           : {}", platform.board.name);
        println!(
            "  cpus            : {} online",
            platform.processes.system().cpus.max(1)
        );
        println!("  exception level : EL{}", platform.cpu.current_el());
        println!("  MIDR_EL1        : {:#018x}", platform.cpu.midr());
        println!(
            "  generic timer   : {} Hz, uptime {} ms",
            platform.clock.frequency(),
            platform.clock.uptime_ms()
        );
        println!("  kernel image    : {kernel_start:#x} .. {kernel_end:#x}");
        println!("  bss             : {bss_start:#x} .. {bss_end:#x}");
        println!(
            "  uart            : {:#x} (console)",
            platform.board.console_base
        );

        match context.display.as_mut() {
            Some(canvas) => println!(
                "  graphics        : ramfb {}x{} XRGB8888 at {:#x}",
                canvas.width(),
                canvas.height(),
                platform.board.framebuffer_addr
            ),
            None => println!("  graphics        : unavailable (no -device ramfb)"),
        }

        match platform.audio {
            Some(audio) if audio.is_ready() => println!(
                "  audio           : Intel HDA {} Hz, {} ch, max {} samples",
                audio.sample_rate(),
                audio.channels(),
                audio.max_samples()
            ),
            _ => println!("  audio           : unavailable (no -device intel-hda)"),
        }
    }
}
