//! The `desktop` shell app.
//!
//! This is the thin board-facing layer around [`feos_desktop::Desktop`]:
//! it embeds the YAML configuration, scans the embedded application folder,
//! starts autostart services and renders one frame. Real input drivers can
//! call the same `Desktop::render` loop and forward the returned launch
//! requests.
//!
//! Applications live in `apps/` next to this crate as single-file `.fapp`
//! bundles. The build script turns every file in that folder into an embedded
//! [`feos_desktop::DirEntry`], so adding a bundle is enough to make it appear
//! in the Start menu.

#![cfg_attr(not(test), no_std)]

use feos_app::{App, Context, Platform};
use feos_console::println;
use feos_desktop::{
    Desktop, DesktopConfig, DesktopResponse, LaunchOutcome, ServiceStatus, StartMenuCommand,
};
use feos_gui::Input;
use feos_sync::SpinLock;

mod folder {
    include!(concat!(env!("OUT_DIR"), "/folder.rs"));
}

pub use folder::FOLDER;

/// Maximum windows, applications and services in a desktop session.
type Session = Desktop<8, 12, 12>;

/// The configuration shipped with the QEMU desktop.
pub const CONFIG_YAML: &str = include_str!("../desktop.yaml");

/// The desktop session lives in `.bss`: the catalog and window table are too
/// large for a typical kernel stack, and keeping one static session lets
/// repeated `desktop` commands preserve windows and services.
static SESSION: SpinLock<Session> = SpinLock::new(Session::new(DesktopConfig::new()));

/// The desktop session app.
pub struct DesktopApp;

impl App for DesktopApp {
    fn name(&self) -> &'static str {
        "desktop"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["wm"]
    }

    fn usage(&self) -> &'static str {
        "desktop [menu|list|status|run <id>|service <start|stop|restart> <id>]"
    }

    fn description(&self) -> &'static str {
        "run the YAML-configured desktop session"
    }

    fn run(&self, context: &mut Context<'_>) {
        let config = match DesktopConfig::parse(CONFIG_YAML) {
            Ok(config) => config,
            Err(error) => {
                println!("desktop: invalid configuration: {error:?}");
                DesktopConfig::default()
            }
        };

        let mut session = SESSION.lock();
        let desktop = &mut *session;
        desktop.set_config(config);
        if config.start_menu.initially_open && !desktop.start_menu().open {
            desktop.start_menu_mut().show();
        }

        let report = desktop.scan(FOLDER);
        println!(
            "[desktop] {} application(s) discovered in the application folder ({} skipped)",
            report.installed, report.skipped
        );

        if !desktop.session_started() {
            let started = desktop.start_session(context.platform.apps, context.platform.processes);
            if started > 0 {
                println!("[desktop] {started} autostart service(s) started");
            }
        }

        let args = context.args.trim();
        if args.eq_ignore_ascii_case("list") {
            list_apps(desktop);
            return;
        }
        if args.eq_ignore_ascii_case("status") {
            print_status(desktop);
            return;
        }
        if let Some(id) = args
            .strip_prefix("run ")
            .or_else(|| args.strip_prefix("start "))
        {
            render(desktop, context);
            run_app(desktop, id.trim(), context.platform);
            render(desktop, context);
            return;
        }
        if let Some(rest) = args.strip_prefix("service ") {
            run_service(desktop, rest.trim(), context.platform);
            return;
        }
        if args.eq_ignore_ascii_case("menu") {
            desktop.start_menu_mut().show();
            render(desktop, context);
            return;
        }
        if !args.is_empty() {
            println!("desktop: unknown option '{args}' (try 'desktop list' or 'desktop status')");
            return;
        }

        render(desktop, context);
    }
}

fn render(desktop: &mut Session, context: &mut Context<'_>) {
    let platform = context.platform;
    let Some(canvas) = context.display.as_mut() else {
        println!("desktop: no framebuffer; start QEMU with -device ramfb");
        return;
    };

    let input = Input::new();
    let uptime = platform.clock.uptime_ms();
    let response = desktop.render(canvas, &input, uptime);
    apply_response(desktop, &response, platform);
    desktop.tick_session(platform.processes);

    println!(
        "[desktop] {}x{} theme={} windows={} start_menu={}",
        canvas.width(),
        canvas.height(),
        desktop.config().theme.name(),
        response.window_count,
        if response.start_open {
            "open"
        } else {
            "closed"
        }
    );
}

fn apply_response(desktop: &mut Session, response: &DesktopResponse, platform: &Platform) {
    if let Some(request) = response.launch {
        match desktop.launch(request.app, platform.apps, platform.processes) {
            LaunchOutcome::Spawned { pid, .. } => {
                println!("[desktop] launched '{}' as pid {pid}", request.entry)
            }
            LaunchOutcome::AlreadyRunning { pid, .. } => {
                println!("[desktop] '{}' is already running ({pid:?})", request.entry)
            }
            LaunchOutcome::UnknownApp => println!("[desktop] launch failed: unknown application"),
            LaunchOutcome::NoWindowSlot => println!("[desktop] launch failed: no free window slot"),
            LaunchOutcome::UnknownEntry => {
                println!(
                    "[desktop] launch failed: entry '{}' is not registered",
                    request.entry
                )
            }
            LaunchOutcome::SpawnFailed(error) => {
                println!("[desktop] launch failed: {error:?}")
            }
        }
    }

    match response.command {
        StartMenuCommand::None => {}
        StartMenuCommand::ShutDown => {
            println!("[desktop] turning the machine off");
            platform.power.system_off();
        }
        StartMenuCommand::LogOff => println!("[desktop] log off requested"),
        StartMenuCommand::Settings => println!("[desktop] desktop settings requested"),
        StartMenuCommand::MyComputer => println!("[desktop] my computer requested"),
        StartMenuCommand::ControlPanel => println!("[desktop] control panel requested"),
        StartMenuCommand::Help => println!("[desktop] help requested"),
        StartMenuCommand::Run => println!("[desktop] run dialog requested"),
        StartMenuCommand::Search => println!("[desktop] search requested"),
    }
}

fn run_app(desktop: &mut Session, id: &str, platform: &Platform) {
    let Some(index) = desktop.catalog().find_id(id) else {
        println!("desktop: no installed application with id '{id}'");
        return;
    };
    match desktop.launch(index, platform.apps, platform.processes) {
        LaunchOutcome::Spawned { window, pid } => {
            println!("[desktop] '{id}' started in window {window} as pid {pid}")
        }
        LaunchOutcome::AlreadyRunning { window, .. } => {
            println!("[desktop] '{id}' is already running in window {window}")
        }
        LaunchOutcome::UnknownEntry => println!("[desktop] '{id}' has no registered entry point"),
        LaunchOutcome::UnknownApp => println!("[desktop] catalog entry vanished"),
        LaunchOutcome::NoWindowSlot => println!("[desktop] no free window slot"),
        LaunchOutcome::SpawnFailed(error) => {
            println!("[desktop] could not spawn '{id}': {error:?}")
        }
    }
}

fn run_service(desktop: &mut Session, request: &str, platform: &Platform) {
    let mut parts = request.split_whitespace();
    let Some(action) = parts.next() else {
        println!("desktop: usage: desktop service <start|stop|restart> <id>");
        return;
    };
    let Some(id) = parts.next() else {
        println!("desktop: usage: desktop service <start|stop|restart> <id>");
        return;
    };

    if !desktop.register_service(id, platform.apps) {
        println!("desktop: service table is full");
        return;
    }

    match action {
        "start" => match desktop.start_service(id, platform.processes) {
            Ok(pid) => println!("[desktop] service '{id}' started as pid {pid}"),
            Err(error) => println!("[desktop] could not start '{id}': {error:?}"),
        },
        "stop" => {
            let stopped = desktop.stop_service(id, platform.processes);
            println!("[desktop] service '{id}' stopped ({stopped})");
        }
        "restart" => match desktop.restart_service(id, platform.processes) {
            Ok(pid) => println!("[desktop] service '{id}' restarted as pid {pid}"),
            Err(error) => println!("[desktop] could not restart '{id}': {error:?}"),
        },
        other => println!("desktop: unknown service action '{other}'"),
    }
}

fn list_apps(desktop: &Session) {
    println!("Applications in the folder:");
    if desktop.catalog().is_empty() {
        println!("  (none) - add a .fapp bundle to apps/desktop/apps");
        return;
    }
    for app in desktop.catalog().iter() {
        println!(
            "  {:<20} {:<14} entry={:<10} {}",
            app.manifest.id.as_str(),
            app.manifest.category.as_str(),
            app.manifest.entry.as_str(),
            app.manifest.description.as_str(),
        );
    }
}

fn print_status(desktop: &Session) {
    let config = desktop.config();
    println!("Desktop status");
    println!("  theme:     {}", config.theme.name());
    println!(
        "  taskbar:   {:?}, {} px, clock={}",
        config.taskbar.position, config.taskbar.height, config.taskbar.show_clock
    );
    println!(
        "  apps:      {} installed from {}",
        desktop.catalog().len(),
        config
            .app_dirs()
            .first()
            .map(|dir| dir.as_str())
            .unwrap_or("(no directory)")
    );
    println!("  windows:   {}", desktop.windows().count());
    println!("  services:  {}", desktop.init().len());

    let mut statuses = [ServiceStatus::EMPTY; 12];
    let count = desktop.services(&mut statuses);
    for status in &statuses[..count] {
        if let Some(pid) = status.pid {
            println!(
                "    {:<16} {:<8} pid={pid:<4} restarts={}",
                status.id.as_str(),
                status.state.name(),
                status.restarts
            );
        } else {
            println!(
                "    {:<16} {:<8} pid=-    restarts={}",
                status.id.as_str(),
                status.state.name(),
                status.restarts
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        let config = DesktopConfig::parse(CONFIG_YAML).expect("shipped configuration parses");
        let mut desktop = Session::new(config);
        let report = desktop.scan(FOLDER);
        assert_eq!(report.installed, FOLDER.len());
        assert_eq!(report.skipped, 0);
        desktop
    }

    #[test]
    fn embedded_folder_loads_every_bundle() {
        let desktop = session();
        assert!(!desktop.catalog().is_empty());
        for app in desktop.catalog().iter() {
            assert!(app.has_icon, "{} has no icon", app.id());
            assert!(!app.manifest.entry.is_empty());
            assert!(!app.manifest.name.is_empty());
        }
    }

    #[test]
    fn shipped_configuration_is_wired_to_the_folder() {
        let desktop = session();
        assert_eq!(desktop.config().theme.name(), "xp-blue");
        assert!(desktop.config().app_dirs()[0].as_str().ends_with("apps"));
        assert!(desktop.catalog().find_id("terminal").is_some());
        assert!(desktop.catalog().find_id("process-monitor").is_some());
    }
}
