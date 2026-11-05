//! The FEOS serial terminal.
//!
//! This crate is a thin adapter between the Bash-compatible `feos-sh` engine
//! and the kernel. The engine owns parsing, expansion, builtins and line
//! editing; `feos_apps::host` runs external commands as processes.

#![no_std]

extern crate alloc;

use feos_app::Platform;
use feos_apps::host::{self, ProcessHost};
use feos_gfx::Canvas;
use feos_sh::{ConsoleInput, ConsoleOutput, Io, Shell};

/// Runs the terminal forever.
///
/// The shell is PID 1, so reaching end-of-file (Ctrl-D) or typing `exit` does
/// not return control to a kernel: it simply starts a fresh prompt.
pub fn run(display: Option<Canvas<'static>>, platform: &'static Platform) -> ! {
    let mut display = display;
    let mut shell = Shell::new(platform.filesystem, host::system(platform));
    let mut host = ProcessHost::serial(platform, &mut display, shell.cancel_token());

    let mut stdin = ConsoleInput;
    let mut stdout = ConsoleOutput;
    let mut stderr = ConsoleOutput;

    loop {
        let mut io = Io {
            stdin: &mut stdin,
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        feos_sh::interactive(&mut shell, &mut host, &mut io);
        feos_console::println!("[shell] starting a fresh prompt (the serial shell is PID 1)");
    }
}
