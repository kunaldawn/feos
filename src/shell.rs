//! A minimal line-oriented serial shell.

use feos_gfx::Canvas;

use crate::arch::{psci, timer};
use crate::drivers::pl011;
use crate::graphics;

const MAX_LINE: usize = 128;
const PROMPT: &str = "feos> ";

extern "C" {
    static __kernel_start: u8;
    static __bss_start: u8;
    static __bss_end: u8;
    static __kernel_end: u8;
}

/// Reads and executes commands forever.
pub fn run(mut display: Option<Canvas<'static>>) -> ! {
    let mut line = [0u8; MAX_LINE];
    let mut len = 0usize;

    print!("{PROMPT}");

    loop {
        let Some(byte) = pl011::getc() else {
            core::hint::spin_loop();
            continue;
        };

        match byte {
            b'\r' | b'\n' => {
                println!();
                if len > 0 {
                    let text = core::str::from_utf8(&line[..len]).unwrap_or("");
                    execute(text.trim(), &mut display);
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
            b if b.is_ascii_graphic() || b == b' ' => {
                if len < MAX_LINE {
                    line[len] = b;
                    len += 1;
                    pl011::putc(b);
                }
            }
            _ => {}
        }
    }
}

fn execute(line: &str, display: &mut Option<Canvas<'static>>) {
    let mut parts = line.splitn(2, ' ');
    let command = parts.next().unwrap_or("");
    let args = parts.next().unwrap_or("").trim();

    match command {
        "" => {}
        "help" => print_help(),
        "echo" => println!("{args}"),
        "info" => print_info(display),
        "gfx" | "demo" => draw_display(display, false),
        "splash" => draw_display(display, true),
        "uptime" => println!("uptime: {} ms", timer::uptime_ms()),
        "sleep" => match args.parse::<u64>() {
            Ok(ms) => {
                timer::spin_ms(ms);
                println!("slept for {ms} ms");
            }
            Err(_) => println!("usage: sleep <milliseconds>"),
        },
        "clear" => print!("\x1b[2J\x1b[H"),
        "shutdown" | "poweroff" => {
            println!("powering off via PSCI...");
            psci::system_off()
        }
        "reboot" | "reset" => {
            println!("resetting via PSCI...");
            psci::system_reset()
        }
        other => println!("unknown command: {other} (type 'help')"),
    }
}

fn print_help() {
    println!("commands:");
    println!("  help                show this help");
    println!("  echo <text>         print text");
    println!("  info                kernel, CPU and display information");
    println!("  gfx                 draw the graphics test card");
    println!("  splash              redraw the boot splash");
    println!("  uptime              milliseconds since boot");
    println!("  sleep <ms>          busy-wait for the given time");
    println!("  clear               clear the terminal");
    println!("  poweroff            power off the machine (exits QEMU)");
    println!("  reboot              reset the machine");
}

fn draw_display(display: &mut Option<Canvas<'static>>, boot_splash: bool) {
    match display.as_mut() {
        Some(canvas) => {
            if boot_splash {
                graphics::demo::splash(canvas);
                println!("redrew the boot splash");
            } else {
                graphics::demo::demo(canvas);
                println!(
                    "drew the test card on a {}x{} ramfb framebuffer",
                    canvas.width(),
                    canvas.height()
                );
            }
        }
        None => println!("no framebuffer; restart QEMU with -device ramfb"),
    }
}

fn print_info(display: &Option<Canvas<'static>>) {
    let (bss_start, bss_end, kernel_end) = unsafe {
        (
            &__bss_start as *const u8 as usize,
            &__bss_end as *const u8 as usize,
            &__kernel_end as *const u8 as usize,
        )
    };
    let kernel_start = unsafe { &__kernel_start as *const u8 as usize };

    println!("feos {}", env!("CARGO_PKG_VERSION"));
    println!("  exception level : EL{}", current_el());
    println!("  MIDR_EL1        : {:#018x}", midr_el1());
    println!(
        "  generic timer   : {} Hz, uptime {} ms",
        timer::frequency(),
        timer::uptime_ms()
    );
    println!("  kernel image    : {kernel_start:#x} .. {kernel_end:#x}");
    println!("  bss             : {bss_start:#x} .. {bss_end:#x}");
    println!("  uart            : 0x0900_0000 (PL011, polled)");
    match display {
        Some(canvas) => println!(
            "  graphics        : ramfb {}x{} XRGB8888 at {:#x}",
            canvas.width(),
            canvas.height(),
            graphics::framebuffer_addr()
        ),
        None => println!("  graphics        : unavailable (no -device ramfb)"),
    }
}

fn current_el() -> u64 {
    let value: u64;
    unsafe { core::arch::asm!("mrs {}, CurrentEL", out(reg) value, options(nomem, nostack)) };
    (value >> 2) & 0x3
}

fn midr_el1() -> u64 {
    let value: u64;
    unsafe { core::arch::asm!("mrs {}, MIDR_EL1", out(reg) value, options(nomem, nostack)) };
    value
}
