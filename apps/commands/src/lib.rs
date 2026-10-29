//! Built-in feos applications.
//!
//! Each app is a module exposing a unit struct that implements
//! [`feos_app::App`]. The [`REGISTRY`] is the single dispatch table used by the
//! shell. To add an app, add a module and one entry here; to ship it as a
//! separate crate, move the module to `apps/<name>` and register it from the
//! kernel instead.

#![no_std]

pub mod clear;
pub mod echo;
pub mod gfx;
pub mod help;
pub mod info;
pub mod power;
pub mod sleep;
pub mod splash;
pub mod tone;
pub mod ui;
pub mod uptime;

use feos_app::App;

/// Every app the shell knows about, in `help` order.
pub static REGISTRY: &[&dyn App] = &[
    &help::Help,
    &echo::Echo,
    &info::Info,
    &gfx::Gfx,
    &splash::Splash,
    &tone::Tone,
    &uptime::Uptime,
    &sleep::Sleep,
    &clear::Clear,
    &power::PowerOff,
    &power::Reboot,
];

/// Finds an app by primary name or alias.
pub fn find(name: &str) -> Option<&'static dyn App> {
    REGISTRY.iter().copied().find(|app| app.matches(name))
}
