//! Built-in feos applications.
//!
//! Each app is a module exposing a unit struct that implements
//! [`feos_app::App`]. The [`REGISTRY`] is the single dispatch table used by the
//! shell. To add an app, add a module and one entry here; to ship it as a
//! separate crate, move the module to `apps/<name>` and register it from the
//! kernel instead.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod audio_worker;
pub mod coreutils;
pub mod decode_worker;
pub mod displayd;
pub mod files;
pub mod fsflush;
pub mod gfx;
pub mod gui;
pub mod host;
pub mod image_viewer;
pub mod info;
pub mod kill;
pub mod music;
pub mod net;
pub mod netd;
pub mod play;
pub mod player;
pub mod playlist;
pub mod power;
pub mod ps;
pub mod rec;
pub mod renice;
pub mod settings;
pub mod sleep;
pub mod spawn;
pub mod splash;
pub mod subtitles;
pub mod taskmgr;
pub mod terminal;
pub mod tick;
pub mod tone;
pub mod ui;

use feos_app::App;

/// Every app the shell knows about, in `help` order.
pub static REGISTRY: &[&dyn App] = &[
    &coreutils::CoreUtils,
    &terminal::Terminal,
    &settings::Settings,
    &info::Info,
    &ps::Ps,
    &taskmgr::TaskManager,
    &spawn::Spawn,
    &kill::Kill,
    &renice::Renice,
    &renice::Taskset,
    &tick::Tick,
    &gfx::Gfx,
    &gui::Gui,
    &files::Files,
    &player::Player,
    &music::Music,
    &image_viewer::ImageViewer,
    &play::Play,
    &rec::Rec,
    &splash::Splash,
    &tone::Tone,
    &sleep::Sleep,
    &net::Ifconfig,
    &net::Ping,
    &net::Nslookup,
    &net::Wget,
    &net::Nc,
    &power::PowerOff,
    &power::Reboot,
    &feos_desktop_app::DesktopApp,
    &feos_dosbox_app::DosBoxApp,
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
    fn shell_builtins_are_not_shadowed_by_apps() {
        for name in ["echo", "clear", "help", "uptime"] {
            assert!(find(name).is_none(), "{name} would never be reached");
        }
    }

    #[test]
    fn desktop_resolves_to_the_session_app() {
        let app = find("desktop").expect("desktop command");
        assert_eq!(app.name(), "desktop");
        assert!(find("wm").is_some());
        assert_eq!(find("gui").map(|app| app.name()), Some("gui"));
    }

    #[test]
    fn player_is_a_foreground_app() {
        let app = find("player").expect("player command");
        assert_eq!(app.name(), "player");
        assert!(app.foreground());
        assert!(find("media-player").is_some());
    }

    #[test]
    fn files_is_a_foreground_app() {
        let app = find("files").expect("files command");
        assert_eq!(app.name(), "files");
        assert!(app.foreground());
        assert!(find("explorer").is_some());
    }

    #[test]
    fn task_manager_is_a_foreground_app() {
        let app = find("taskmgr").expect("task manager command");
        assert_eq!(app.name(), "taskmgr");
        assert!(app.foreground());
        assert!(find("task-manager").is_some());
        assert!(find("taskmgr.exe").is_some());
    }
}
