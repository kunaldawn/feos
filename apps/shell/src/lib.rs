//! The FEOS terminal.
//!
//! This crate is a thin adapter between the Bash-compatible `feos-sh` engine
//! and the kernel's app registry. The engine owns parsing, expansion, builtins
//! and line editing; this file supplies console streams, the external-command
//! host, and the platform facts used by the system utilities.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use feos_app::{Context, Platform};
use feos_coreutils::{MemoryInfo as CoreMemoryInfo, System as CoreSystem};
use feos_gfx::Canvas;
use feos_sh::{ConsoleInput, ConsoleOutput, Host, Io, Shell};

struct PlatformSystem<'a> {
    platform: &'a Platform,
}

impl CoreSystem for PlatformSystem<'_> {
    fn sysname(&self) -> &'static str {
        "FEOS"
    }

    fn nodename(&self) -> &'static str {
        "feos"
    }

    fn release(&self) -> &'static str {
        feos_app::VERSION
    }

    fn version(&self) -> &'static str {
        "feos"
    }

    fn machine(&self) -> &'static str {
        self.platform.board.arch
    }

    fn username(&self) -> &'static str {
        "feos"
    }

    fn uid(&self) -> u32 {
        1000
    }

    fn gid(&self) -> u32 {
        1000
    }

    fn uptime_ms(&self) -> u64 {
        self.platform.clock.uptime_ms()
    }

    fn clock_hz(&self) -> u64 {
        self.platform.clock.frequency()
    }

    fn cpu_count(&self) -> usize {
        self.platform.processes.system().cpus.max(1)
    }

    fn pid(&self) -> u32 {
        self.platform.processes.current()
    }

    fn memory(&self) -> Option<CoreMemoryInfo> {
        self.platform.memory.map(|memory| {
            let info = memory.info();
            CoreMemoryInfo {
                total: info.total_bytes,
                used: info.used_bytes,
                free: info.free_bytes,
            }
        })
    }
}

struct AppHost<'a> {
    platform: &'a Platform,
    display: &'a mut Option<Canvas<'static>>,
}

impl Host for AppHost<'_> {
    fn run_external(
        &mut self,
        name: &str,
        args: &[String],
        _stdin: &mut dyn feos_coreutils::Input,
        _stdout: &mut dyn feos_coreutils::Output,
        _stderr: &mut dyn feos_coreutils::Output,
        background: bool,
    ) -> Option<i32> {
        let app = feos_apps::find(name)?;
        let args_text = args.join(" ");

        if background {
            return Some(match self.platform.processes.spawn(app, &args_text) {
                Ok(pid) => {
                    feos_console::println!("[shell] {name} started as PID {pid}");
                    0
                }
                Err(feos_app::SpawnError::NoFreeSlot) => {
                    feos_console::println!("[shell] {name}: no free process slot");
                    1
                }
                Err(feos_app::SpawnError::ArgsTooLong) => {
                    feos_console::println!("[shell] {name}: arguments too long");
                    1
                }
            });
        }

        let mut context = Context {
            args: &args_text,
            display: self.display,
            viewport: None,
            suspend: None,
            suspend_area: None,
            platform: self.platform,
        };
        app.run(&mut context);
        Some(0)
    }

    fn command_names(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        for app in feos_apps::REGISTRY {
            names.push(app.name());
            names.extend(app.aliases().iter().copied());
        }
        names
    }
}

/// Runs the terminal forever.
///
/// The shell is PID 1, so reaching end-of-file (Ctrl-D) or typing `exit` does
/// not return control to a kernel: it simply starts a fresh prompt.
pub fn run(display: Option<Canvas<'static>>, platform: &Platform) -> ! {
    let mut display = display;
    let system = PlatformSystem { platform };
    let mut shell = Shell::new(platform.filesystem, &system);
    let mut host = AppHost {
        platform,
        display: &mut display,
    };

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
