//! `poweroff` and `reboot`: system control.

use feos_app::{App, Context};
use feos_console::println;

/// Powers the machine off.
pub struct PowerOff;

impl App for PowerOff {
    fn name(&self) -> &'static str {
        "poweroff"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["shutdown"]
    }

    fn usage(&self) -> &'static str {
        "poweroff"
    }

    fn description(&self) -> &'static str {
        "power off the machine (exits QEMU)"
    }

    fn run(&self, context: &mut Context<'_>) {
        println!("powering off via PSCI...");
        context.platform.power.system_off()
    }
}

/// Resets the machine.
pub struct Reboot;

impl App for Reboot {
    fn name(&self) -> &'static str {
        "reboot"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["reset"]
    }

    fn usage(&self) -> &'static str {
        "reboot"
    }

    fn description(&self) -> &'static str {
        "reset the machine"
    }

    fn run(&self, context: &mut Context<'_>) {
        println!("resetting via PSCI...");
        context.platform.power.system_reset()
    }
}
