//! Built-in feos applications.
//!
//! Each app is a module exposing a unit struct that implements
//! [`feos_app::App`]. The [`REGISTRY`] is the single dispatch table used by the
//! shell. To add an app, add a module and one entry here; to ship it as a
//! separate crate, move the module to `apps/<name>` and register it from the
//! kernel instead.

#![cfg_attr(not(test), no_std)]

pub mod clear;
pub mod echo;
pub mod gfx;
pub mod gui;
pub mod help;
pub mod info;
pub mod kill;
pub mod power;
pub mod ps;
pub mod sleep;
pub mod spawn;
pub mod splash;
pub mod tick;
pub mod tone;
pub mod ui;
pub mod uptime;

use feos_app::App;

/// Every app the shell knows about, in `help` order.
pub static REGISTRY: &[&dyn App] = &[
    &help::Help,
    &echo::Echo,
    &info::Info,
    &ps::Ps,
    &spawn::Spawn,
    &kill::Kill,
    &tick::Tick,
    &gfx::Gfx,
    &gui::Gui,
    &splash::Splash,
    &tone::Tone,
    &uptime::Uptime,
    &sleep::Sleep,
    &clear::Clear,
    &power::PowerOff,
    &power::Reboot,
    &feos_desktop_app::DesktopApp,
];

/// Finds an app by primary name or alias.
pub fn find(name: &str) -> Option<&'static dyn App> {
    REGISTRY.iter().copied().find(|app| app.matches(name))
}

/// The [`feos_app::AppRegistry`] view of [`REGISTRY`].
///
/// The kernel publishes this through `Platform::apps`, so libraries that must
/// not depend on the app crate (the desktop and init manager) can resolve a
/// manifest entry point at run time.
pub struct Registry;

impl feos_app::AppRegistry for Registry {
    fn find(&self, name: &str) -> Option<&'static dyn App> {
        find(name)
    }
}

/// The shared registry value referenced by [`feos_app::Platform::apps`].
pub static APP_REGISTRY: Registry = Registry;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_names_and_aliases_are_unique() {
        for app in REGISTRY {
            for name in core::iter::once(app.name()).chain(app.aliases().iter().copied()) {
                let matches = REGISTRY.iter().filter(|other| other.matches(name)).count();
                assert_eq!(matches, 1, "command '{name}' is claimed more than once");
            }
        }
    }

    #[test]
    fn desktop_resolves_to_the_session_app() {
        let app = find("desktop").expect("desktop command");
        assert_eq!(app.name(), "desktop");
        assert!(find("wm").is_some());
        assert_eq!(find("gui").map(|app| app.name()), Some("gui"));
    }
}
