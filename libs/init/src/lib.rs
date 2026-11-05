//! A small init process manager and service supervisor.
//!
//! The kernel already provides preemptive processes; this crate adds the
//! policy layer a desktop session needs:
//!
//! - a list of autostart services read from YAML,
//! - name-to-app resolution through [`AppRegistry`],
//! - start, stop and restart by service id,
//! - restart policies with a bounded retry count and growing backoff,
//! - snapshots for a `ps`-style status view.
//!
//! It is deliberately independent of the desktop and of the shell registry.
//! The desktop passes its registry in, and the `init` command does the same.
//!
//! ```no_run
//! use feos_init::{Init, RestartPolicy};
//!
//! let mut init = Init::<8>::new(RestartPolicy::OnFailure, 3);
//! // init.register("terminal", &TERMINAL, "");
//! // init.start("terminal", &platform.processes);
//! // init.tick(&platform.processes);
//! ```

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use feos_app::{App, AppRegistry, ProcessInfo, ProcessState, Processes, SpawnError, MAX_PROCESSES};
use feos_text::Text;
use feos_yaml::{YamlDocument, YamlError, YamlNode};

/// Default maximum number of supervised services.
pub const MAX_SERVICES: usize = 12;

/// Maximum length of a service id.
pub const SERVICE_ID_MAX: usize = 32;

/// Maximum length of service arguments.
pub const SERVICE_ARGS_MAX: usize = 64;

/// Maximum number of autostart ids in a [`Config`].
pub const AUTOSTART_MAX: usize = 8;

/// YAML nodes [`Config::parse`] can hold, more than any shipped configuration.
const CONFIG_NODES: usize = 256;

/// Delay before a second automatic restart. It doubles with each further one.
/// The first restart is immediate.
pub const RESTART_BACKOFF_MS: u64 = 500;

/// Longest delay between automatic restarts.
pub const RESTART_BACKOFF_MAX_MS: u64 = 30_000;

/// A process that stays up this long has its restart budget refilled.
pub const HEALTHY_RUN_MS: u64 = 60_000;

/// Delay before retrying a spawn that failed because the process table is full.
pub const SPAWN_RETRY_MS: u64 = 1_000;

/// Time [`Init::tick`] assumes has passed since its last call.
const ASSUMED_TICK_MS: u64 = 20;

/// What to do when a supervised process exits.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RestartPolicy {
    /// Leave the service stopped.
    Never,
    /// Restart after the process disappears (feos has no exit status yet, so
    /// every exit is treated as a failure).
    #[default]
    OnFailure,
    /// Restart unconditionally.
    Always,
}

impl RestartPolicy {
    /// Parses `never`, `on-failure`/`on_failure`/`onfailure`, or `always`.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.eq_ignore_ascii_case("never") || value.eq_ignore_ascii_case("no") {
            Some(Self::Never)
        } else if value.eq_ignore_ascii_case("always") || value.eq_ignore_ascii_case("yes") {
            Some(Self::Always)
        } else if value.eq_ignore_ascii_case("on-failure")
            || value.eq_ignore_ascii_case("on_failure")
            || value.eq_ignore_ascii_case("onfailure")
        {
            Some(Self::OnFailure)
        } else {
            None
        }
    }

    /// A short name suitable for configuration output.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::OnFailure => "on-failure",
            Self::Always => "always",
        }
    }
}

/// Lifecycle state of one supervised service.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ServiceState {
    /// Registered but not running.
    Stopped,
    /// Running under a PID.
    Running,
    /// The entry point was not found in the registry.
    Missing,
    /// The restart budget was exhausted.
    Failed,
}

impl ServiceState {
    /// A short name suitable for a status table.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Running => "running",
            Self::Missing => "missing",
            Self::Failed => "failed",
        }
    }
}

/// Errors from explicit service operations.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InitError {
    /// Every service slot is in use.
    NoFreeSlot,
    /// The service id is not registered.
    UnknownService,
    /// The manifest entry point is not present in the registry.
    UnknownApp,
    /// The kernel refused to create a process.
    Spawn(SpawnError),
    /// The service already has a live process.
    AlreadyRunning,
    /// A stop was requested but the process has not gone yet.
    StopPending,
    /// The app is windowed, so it only does useful work when the desktop opens
    /// its window: a background process for it would exit straight away. See
    /// [`Init::windowed_services`].
    Windowed,
}

/// Why an `init:` configuration was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConfigError {
    /// The YAML itself is malformed.
    Yaml(YamlError),
    /// `autostart` lists more than [`AUTOSTART_MAX`] services.
    TooManyAutostart,
    /// An autostart id is longer than [`SERVICE_ID_MAX`], or isn't text.
    InvalidAutostartId,
}

impl From<YamlError> for ConfigError {
    fn from(error: YamlError) -> Self {
        Self::Yaml(error)
    }
}

/// A snapshot of one service for status views.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ServiceStatus {
    /// Service identifier.
    pub id: Text<SERVICE_ID_MAX>,
    /// Process id while running.
    pub pid: Option<u32>,
    /// Current lifecycle state.
    pub state: ServiceState,
    /// Number of automatic restarts performed.
    pub restarts: u8,
}

impl ServiceStatus {
    /// An empty status entry.
    pub const EMPTY: Self = Self {
        id: Text::EMPTY,
        pid: None,
        state: ServiceState::Stopped,
        restarts: 0,
    };
}

/// The `init:` section of a desktop or system configuration.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// Service ids to start automatically.
    pub autostart: [Text<SERVICE_ID_MAX>; AUTOSTART_MAX],
    /// Number of valid entries in [`autostart`](Config::autostart).
    pub autostart_len: usize,
    /// Restart policy for supervised services.
    pub policy: RestartPolicy,
    /// Maximum automatic restarts per service.
    pub max_restarts: u8,
}

impl Config {
    /// The default configuration: no autostart, restart on failure, 3 retries.
    pub const fn new() -> Self {
        Self {
            autostart: [Text::EMPTY; AUTOSTART_MAX],
            autostart_len: 0,
            policy: RestartPolicy::OnFailure,
            max_restarts: 3,
        }
    }

    /// Parses the `init:` mapping from `yaml`.
    ///
    /// When the document has no `init:` key the root is used, so the same
    /// function accepts a standalone init file or the desktop configuration.
    ///
    /// Input that can't be represented is an error, reported here as
    /// [`YamlError::BufferTooSmall`]. Use [`Config::try_parse`] to tell the
    /// cases apart, or [`Config::from_document`] to share a parsed document.
    pub fn parse(yaml: &str) -> Result<Self, YamlError> {
        Self::try_parse(yaml).map_err(|error| match error {
            ConfigError::Yaml(error) => error,
            _ => YamlError::BufferTooSmall,
        })
    }

    /// Like [`Config::parse`], with the precise reason for a refusal.
    pub fn try_parse(yaml: &str) -> Result<Self, ConfigError> {
        let mut nodes = [YamlNode::EMPTY; CONFIG_NODES];
        let document = YamlDocument::parse(yaml, &mut nodes)?;
        Self::from_document(&document)
    }

    /// Reads the `init:` mapping from an already parsed document.
    ///
    /// A caller that parsed the document for other settings should use this: it
    /// needs no second arena, and has the same node capacity as the caller's.
    pub fn from_document(document: &YamlDocument<'_, '_>) -> Result<Self, ConfigError> {
        let root = document.root();
        let section = document.get(root, "init").unwrap_or(root);
        let mut config = Self::new();

        let mut policy = [0u8; 16];
        if let Some(policy) = document
            .get(section, "restart")
            .and_then(|node| document.as_str(node, &mut policy))
            .and_then(RestartPolicy::parse)
        {
            config.policy = policy;
        }
        if let Some(value) = document
            .get(section, "max_restarts")
            .and_then(|node| document.as_u64(node))
        {
            config.max_restarts = value.min(u8::MAX as u64) as u8;
        }
        if let Some(autostart) = document.get(section, "autostart") {
            for node in document.sequence_iter(autostart).into_iter().flatten() {
                let mut buffer = [0u8; SERVICE_ID_MAX * 2];
                let value = document
                    .as_str(node, &mut buffer)
                    .ok_or(ConfigError::InvalidAutostartId)?
                    .trim();
                if value.is_empty() {
                    continue;
                }
                if config.autostart_len == config.autostart.len() {
                    return Err(ConfigError::TooManyAutostart);
                }
                if !config.autostart[config.autostart_len].set(value) {
                    return Err(ConfigError::InvalidAutostartId);
                }
                config.autostart_len += 1;
            }
        }

        Ok(config)
    }

    /// The configured autostart ids.
    pub fn autostart(&self) -> &[Text<SERVICE_ID_MAX>] {
        &self.autostart[..self.autostart_len]
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy)]
struct Service {
    id: Text<SERVICE_ID_MAX>,
    app: Option<&'static dyn App>,
    args: Text<SERVICE_ARGS_MAX>,
    pid: Option<u32>,
    state: ServiceState,
    restarts: u8,
    /// The app runs in a desktop window, not as a supervised process.
    windowed: bool,
    /// A stop was requested and `pid` hasn't gone yet.
    stopping: bool,
    /// When a pending automatic restart is due.
    restart_at: Option<u64>,
    /// When the current process was started.
    started_at: u64,
}

impl Service {
    const EMPTY: Self = Self {
        id: Text::EMPTY,
        app: None,
        args: Text::EMPTY,
        pid: None,
        state: ServiceState::Stopped,
        restarts: 0,
        windowed: false,
        stopping: false,
        restart_at: None,
        started_at: 0,
    };

    /// The state a service with no process settles in.
    fn idle_state(&self) -> ServiceState {
        if self.app.is_some() {
            ServiceState::Stopped
        } else {
            ServiceState::Missing
        }
    }
}

/// A fixed-capacity init/service manager.
pub struct Init<const N: usize = MAX_SERVICES> {
    services: [Service; N],
    len: usize,
    policy: RestartPolicy,
    max_restarts: u8,
    now_ms: u64,
}

impl<const N: usize> Init<N> {
    /// Creates an empty manager with the given restart policy.
    pub const fn new(policy: RestartPolicy, max_restarts: u8) -> Self {
        Self {
            services: [Service::EMPTY; N],
            len: 0,
            policy,
            max_restarts,
            now_ms: 0,
        }
    }

    /// The restart policy in use.
    pub const fn policy(&self) -> RestartPolicy {
        self.policy
    }

    /// Replaces the restart policy.
    pub fn set_policy(&mut self, policy: RestartPolicy) {
        self.policy = policy;
    }

    /// Number of registered services.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` when no services are registered.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Registers an application under `id`.
    ///
    /// Returns `false` when the id already exists, the table is full, or the id
    /// or arguments don't fit. A cut-short id could never be found again, and
    /// cut-short arguments would run something else.
    ///
    /// A [windowed](App::windowed) app is recorded but never spawned; see
    /// [`Init::windowed_services`].
    pub fn register(&mut self, id: &str, app: &'static dyn App, args: &str) -> bool {
        if id.is_empty() || self.find(id).is_some() || self.len == N {
            return false;
        }
        let mut service = Service::EMPTY;
        if !service.id.set(id) || !service.args.set(args) {
            return false;
        }
        service.app = Some(app);
        service.windowed = app.windowed();
        self.services[self.len] = service;
        self.len += 1;
        true
    }

    /// Registers an id that has no matching app yet.
    ///
    /// The service appears in status output as `missing` and can never start.
    pub fn register_missing(&mut self, id: &str) -> bool {
        if id.is_empty() || self.find(id).is_some() || self.len == N {
            return false;
        }
        let mut service = Service::EMPTY;
        if !service.id.set(id) {
            return false;
        }
        service.state = ServiceState::Missing;
        self.services[self.len] = service;
        self.len += 1;
        true
    }

    /// Registers every autostart id using `registry`, recording missing ones.
    ///
    /// Returns the number of ids that resolved to an app.
    pub fn register_autostart(&mut self, registry: &dyn AppRegistry, config: &Config) -> usize {
        let mut resolved = 0;
        for id in config.autostart() {
            if let Some(app) = registry.find(id.as_str()) {
                if self.register(id.as_str(), app, "") {
                    resolved += 1;
                }
            } else {
                self.register_missing(id.as_str());
            }
        }
        resolved
    }

    /// Finds a service index by id.
    pub fn find(&self, id: &str) -> Option<usize> {
        self.services[..self.len]
            .iter()
            .position(|service| service.id.eq_str(id))
    }

    /// Returns the status of one service.
    pub fn status_of(&self, id: &str) -> Option<ServiceStatus> {
        let index = self.find(id)?;
        Some(self.snapshot_one(index))
    }

    /// Ids of registered services whose app is windowed.
    ///
    /// These can't be supervised as processes. The desktop should open a window
    /// for each one at session start instead.
    pub fn windowed_services(&self) -> impl Iterator<Item = &str> {
        self.services[..self.len]
            .iter()
            .filter(|service| service.windowed)
            .map(|service| service.id.as_str())
    }

    /// Starts a service, which also refills its restart budget.
    ///
    /// A service that still has a live process is refused, so there is never a
    /// second instance and a forgotten PID.
    pub fn start(&mut self, id: &str, processes: &dyn Processes) -> Result<u32, InitError> {
        let index = self.find(id).ok_or(InitError::UnknownService)?;
        self.start_index(index, processes)
    }

    fn start_index(&mut self, index: usize, processes: &dyn Processes) -> Result<u32, InitError> {
        let now = self.now_ms;
        let service = &mut self.services[index];
        let app = service.app.ok_or(InitError::UnknownApp)?;
        if service.windowed {
            return Err(InitError::Windowed);
        }
        if let Some(pid) = service.pid {
            if pid_is_live(processes, pid) {
                return Err(if service.stopping {
                    InitError::StopPending
                } else {
                    InitError::AlreadyRunning
                });
            }
            service.pid = None;
            service.stopping = false;
        }
        let pid = processes
            .spawn(app, service.args.as_str())
            .map_err(InitError::Spawn)?;
        service.pid = Some(pid);
        service.state = ServiceState::Running;
        service.restarts = 0;
        service.restart_at = None;
        service.started_at = now;
        Ok(pid)
    }

    /// Starts every registered service that has an app.
    ///
    /// Returns the number of services that started successfully. Windowed apps
    /// are left to the desktop.
    pub fn start_all(&mut self, processes: &dyn Processes) -> usize {
        let mut started = 0;
        for index in 0..self.len {
            if self.services[index].state == ServiceState::Running {
                continue;
            }
            if self.services[index].app.is_none() {
                continue;
            }
            if self.start_index(index, processes).is_ok() {
                started += 1;
            }
        }
        started
    }

    /// Stops a service. Returns `true` when a running process was killed.
    ///
    /// A kill can fail or take effect later. Until the process has really gone
    /// the service keeps its PID and stays `Running`, and [`Init::tick`] keeps
    /// asking; it settles to `Stopped` once the process disappears, and is not
    /// restarted.
    pub fn stop(&mut self, id: &str, processes: &dyn Processes) -> bool {
        let Some(index) = self.find(id) else {
            return false;
        };
        self.stop_index(index, processes)
    }

    fn stop_index(&mut self, index: usize, processes: &dyn Processes) -> bool {
        let service = &mut self.services[index];
        service.restart_at = None;
        let Some(pid) = service.pid else {
            service.state = service.idle_state();
            return false;
        };
        let killed = processes.kill(pid);
        if pid_is_live(processes, pid) {
            service.stopping = true;
        } else {
            service.pid = None;
            service.stopping = false;
            service.state = service.idle_state();
        }
        killed
    }

    /// Restarts a service, returning its new PID.
    ///
    /// Fails with [`InitError::StopPending`] while the old process is still
    /// alive, rather than creating a second instance. Call again later.
    pub fn restart(&mut self, id: &str, processes: &dyn Processes) -> Result<u32, InitError> {
        let index = self.find(id).ok_or(InitError::UnknownService)?;
        self.stop_index(index, processes);
        self.start_index(index, processes)
    }

    /// Checks every service and applies the restart policy, assuming roughly
    /// one desktop frame has passed. Prefer [`Init::tick_at`] when a clock is
    /// at hand, since backoff is measured in time.
    ///
    /// Returns the number of services that exited, restarted or moved to
    /// `Failed`.
    pub fn tick(&mut self, processes: &dyn Processes) -> usize {
        self.tick_at(processes, self.now_ms.saturating_add(ASSUMED_TICK_MS))
    }

    /// Checks every service at time `now_ms`, on any monotonic clock.
    ///
    /// The first restart after a healthy start is immediate, and later ones wait
    /// [`RESTART_BACKOFF_MS`], doubling up to [`RESTART_BACKOFF_MAX_MS`]. A run
    /// of [`HEALTHY_RUN_MS`] refills the budget. A full process table is retried
    /// and doesn't use up the budget.
    pub fn tick_at(&mut self, processes: &dyn Processes, now_ms: u64) -> usize {
        self.now_ms = self.now_ms.max(now_ms);
        let now = self.now_ms;
        if self.len == 0 {
            return 0;
        }

        let mut live = [ProcessInfo::EMPTY; MAX_PROCESSES];
        let count = processes.snapshot(&mut live);
        let mut changed = 0;

        for index in 0..self.len {
            let service = &mut self.services[index];
            let mut exited = false;

            if let Some(pid) = service.pid {
                let healthy = now.saturating_sub(service.started_at) >= HEALTHY_RUN_MS;
                if healthy {
                    service.restarts = 0;
                }
                if process_is_live(&live[..count], pid) {
                    if service.stopping {
                        processes.kill(pid);
                    }
                    continue;
                }

                service.pid = None;
                exited = true;
                changed += 1;

                if service.stopping {
                    service.stopping = false;
                    service.state = service.idle_state();
                    continue;
                }
                if self.policy == RestartPolicy::Never {
                    service.state = ServiceState::Stopped;
                    continue;
                }
                if service.restarts >= self.max_restarts {
                    service.state = ServiceState::Failed;
                    continue;
                }
                if service.app.is_none() {
                    service.state = ServiceState::Missing;
                    continue;
                }
                service.state = ServiceState::Stopped;
                service.restart_at = Some(now.saturating_add(backoff_ms(service.restarts)));
            }

            let Some(due) = service.restart_at else {
                continue;
            };
            let Some(app) = service.app else {
                continue;
            };
            if now < due {
                continue;
            }
            match processes.spawn(app, service.args.as_str()) {
                Ok(new_pid) => {
                    service.pid = Some(new_pid);
                    service.restarts = service.restarts.saturating_add(1);
                    service.state = ServiceState::Running;
                    service.restart_at = None;
                    service.started_at = now;
                    if !exited {
                        changed += 1;
                    }
                }
                Err(SpawnError::NoFreeSlot) => {
                    service.restart_at = Some(now.saturating_add(SPAWN_RETRY_MS));
                }
                Err(_) => {
                    service.state = ServiceState::Failed;
                    service.restart_at = None;
                    if !exited {
                        changed += 1;
                    }
                }
            }
        }

        changed
    }

    /// Copies service statuses into `out` and returns the number written.
    pub fn snapshot(&self, out: &mut [ServiceStatus]) -> usize {
        let count = self.len.min(out.len());
        for index in 0..count {
            out[index] = self.snapshot_one(index);
        }
        count
    }

    /// Status of the service at `index`.
    pub fn status_at(&self, index: usize) -> Option<ServiceStatus> {
        if index < self.len {
            Some(self.snapshot_one(index))
        } else {
            None
        }
    }

    fn snapshot_one(&self, index: usize) -> ServiceStatus {
        let service = &self.services[index];
        ServiceStatus {
            id: service.id,
            pid: service.pid,
            state: service.state,
            restarts: service.restarts,
        }
    }
}

/// Delay before the next automatic restart, after `restarts` so far.
fn backoff_ms(restarts: u8) -> u64 {
    match restarts {
        0 => 0,
        count => RESTART_BACKOFF_MS
            .saturating_mul(1u64 << (count - 1).min(16))
            .min(RESTART_BACKOFF_MAX_MS),
    }
}

fn pid_is_live(processes: &dyn Processes, pid: u32) -> bool {
    let mut live = [ProcessInfo::EMPTY; MAX_PROCESSES];
    let count = processes.snapshot(&mut live);
    process_is_live(&live[..count], pid)
}

fn process_is_live(processes: &[ProcessInfo], pid: u32) -> bool {
    processes
        .iter()
        .any(|process| process.pid == pid && process.state != ProcessState::Exited)
}

#[cfg(test)]
mod tests {
    use super::*;
    use feos_app::Context;
    use feos_sync::SpinLock;

    struct Dummy;

    impl App for Dummy {
        fn name(&self) -> &'static str {
            "dummy"
        }

        fn usage(&self) -> &'static str {
            "dummy"
        }

        fn description(&self) -> &'static str {
            "test app"
        }

        fn run(&self, _context: &mut Context<'_>) -> i32 {
        0
    }
    }

    static DUMMY: Dummy = Dummy;

    struct WindowedDummy;

    impl App for WindowedDummy {
        fn name(&self) -> &'static str {
            "windowed"
        }

        fn usage(&self) -> &'static str {
            "windowed"
        }

        fn description(&self) -> &'static str {
            "test app with a window"
        }

        fn windowed(&self) -> bool {
            true
        }

        fn run(&self, _context: &mut Context<'_>) -> i32 {
        0
    }
    }

    static WINDOWED: WindowedDummy = WindowedDummy;

    #[derive(Default)]
    struct FakeState {
        next_pid: u32,
        live: [u32; 8],
        live_len: usize,
        /// Kills report failure and leave the process alive.
        refuse_kill: bool,
        /// Spawns fail with this error.
        spawn_error: Option<SpawnError>,
    }

    struct FakeProcesses {
        state: SpinLock<FakeState>,
    }

    impl FakeProcesses {
        fn new() -> Self {
            Self {
                state: SpinLock::new(FakeState {
                    next_pid: 2,
                    live: [0; 8],
                    live_len: 0,
                    refuse_kill: false,
                    spawn_error: None,
                }),
            }
        }

        fn exit(&self, pid: u32) {
            let mut state = self.state.lock();
            let live_len = state.live_len;
            if let Some(index) = state.live[..live_len]
                .iter()
                .position(|&value| value == pid)
            {
                state.live.copy_within(index + 1..live_len, index);
                state.live_len = live_len - 1;
            }
        }

        fn spawned(&self) -> u32 {
            self.state.lock().next_pid - 2
        }
    }

    impl Processes for FakeProcesses {
        fn spawn(&self, _app: &'static dyn App, _args: &str) -> Result<u32, SpawnError> {
            let mut state = self.state.lock();
            if let Some(error) = state.spawn_error {
                return Err(error);
            }
            let live_len = state.live_len;
            if live_len == state.live.len() {
                return Err(SpawnError::NoFreeSlot);
            }
            let pid = state.next_pid;
            state.next_pid += 1;
            state.live[live_len] = pid;
            state.live_len = live_len + 1;
            Ok(pid)
        }

        fn kill(&self, pid: u32) -> bool {
            let mut state = self.state.lock();
            if state.refuse_kill {
                return false;
            }
            let live_len = state.live_len;
            if let Some(index) = state.live[..live_len]
                .iter()
                .position(|&value| value == pid)
            {
                state.live.copy_within(index + 1..live_len, index);
                state.live_len = live_len - 1;
                true
            } else {
                false
            }
        }

        fn current(&self) -> u32 {
            1
        }

        fn snapshot(&self, out: &mut [ProcessInfo]) -> usize {
            let state = self.state.lock();
            let count = state.live_len.min(out.len());
            for (index, &pid) in state.live[..count].iter().enumerate() {
                out[index] = ProcessInfo {
                    pid,
                    state: ProcessState::Running,
                    ..ProcessInfo::EMPTY
                };
            }
            count
        }
    }

    #[test]
    fn parses_init_section() {
        let config = Config::parse(concat!(
            "theme: xp-blue\n",
            "init:\n",
            "  restart: always\n",
            "  max_restarts: 5\n",
            "  autostart:\n",
            "    - terminal\n",
            "    - files\n",
        ))
        .unwrap();
        assert_eq!(config.policy, RestartPolicy::Always);
        assert_eq!(config.max_restarts, 5);
        assert_eq!(config.autostart_len, 2);
        assert_eq!(config.autostart()[1].as_str(), "files");
    }

    #[test]
    fn starts_and_restarts_services() {
        let processes = FakeProcesses::new();
        let mut init = Init::<4>::new(RestartPolicy::OnFailure, 2);
        assert!(init.register("dummy", &DUMMY, ""));
        assert!(init.register_missing("missing"));
        assert_eq!(init.len(), 2);

        let pid = init.start("dummy", &processes).unwrap();
        assert_eq!(pid, 2);
        assert_eq!(
            init.status_of("dummy").unwrap().state,
            ServiceState::Running
        );

        processes.exit(pid);
        assert_eq!(init.tick(&processes), 1);
        let status = init.status_of("dummy").unwrap();
        assert_eq!(status.state, ServiceState::Running);
        assert_eq!(status.restarts, 1);
        assert_eq!(processes.spawned(), 2);
        assert_eq!(
            init.status_of("missing").unwrap().state,
            ServiceState::Missing
        );
        assert_eq!(
            init.start("missing", &processes),
            Err(InitError::UnknownApp)
        );
    }

    #[test]
    fn stops_after_restart_budget() {
        let processes = FakeProcesses::new();
        let mut init = Init::<4>::new(RestartPolicy::Always, 1);
        init.register("dummy", &DUMMY, "");
        let pid = init.start("dummy", &processes).unwrap();
        processes.exit(pid);
        init.tick(&processes);
        let pid = init.status_of("dummy").unwrap().pid.unwrap();
        processes.exit(pid);
        init.tick(&processes);
        assert_eq!(init.status_of("dummy").unwrap().state, ServiceState::Failed);
    }

    #[test]
    fn stop_and_restart() {
        let processes = FakeProcesses::new();
        let mut init = Init::<4>::new(RestartPolicy::Never, 0);
        init.register("dummy", &DUMMY, "");
        let pid = init.start("dummy", &processes).unwrap();
        assert!(init.stop("dummy", &processes));
        assert_eq!(
            init.status_of("dummy").unwrap().state,
            ServiceState::Stopped
        );
        let new_pid = init.restart("dummy", &processes).unwrap();
        assert_ne!(pid, new_pid);
        assert_eq!(
            init.status_of("dummy").unwrap().state,
            ServiceState::Running
        );
    }

    #[test]
    fn starting_twice_is_refused() {
        let processes = FakeProcesses::new();
        let mut init = Init::<4>::new(RestartPolicy::Never, 0);
        init.register("dummy", &DUMMY, "");
        let pid = init.start("dummy", &processes).unwrap();
        assert_eq!(
            init.start("dummy", &processes),
            Err(InitError::AlreadyRunning)
        );
        assert_eq!(processes.spawned(), 1);
        assert_eq!(init.status_of("dummy").unwrap().pid, Some(pid));

        // A process that died without a tick noticing doesn't block a start.
        processes.exit(pid);
        assert!(init.start("dummy", &processes).is_ok());
    }

    #[test]
    fn failed_kill_keeps_the_pid_until_the_process_goes() {
        let processes = FakeProcesses::new();
        let mut init = Init::<4>::new(RestartPolicy::Always, 3);
        init.register("dummy", &DUMMY, "");
        let pid = init.start("dummy", &processes).unwrap();

        processes.state.lock().refuse_kill = true;
        assert!(!init.stop("dummy", &processes));
        let status = init.status_of("dummy").unwrap();
        assert_eq!(status.pid, Some(pid));
        assert_eq!(status.state, ServiceState::Running);

        // No orphan and no second instance.
        assert_eq!(
            init.restart("dummy", &processes),
            Err(InitError::StopPending)
        );
        assert_eq!(init.start("dummy", &processes), Err(InitError::StopPending));
        assert_eq!(processes.spawned(), 1);

        // `tick` keeps asking, and once it works the service stays stopped even
        // though the policy says always restart.
        init.tick(&processes);
        assert_eq!(init.status_of("dummy").unwrap().pid, Some(pid));
        processes.state.lock().refuse_kill = false;
        init.tick(&processes);
        init.tick(&processes);
        let status = init.status_of("dummy").unwrap();
        assert_eq!(status.pid, None);
        assert_eq!(status.state, ServiceState::Stopped);
        assert_eq!(processes.spawned(), 1);

        assert!(init.restart("dummy", &processes).is_ok());
        assert_eq!(processes.spawned(), 2);
    }

    #[test]
    fn restarts_back_off_and_the_budget_recovers() {
        let processes = FakeProcesses::new();
        let mut init = Init::<4>::new(RestartPolicy::OnFailure, 3);
        init.register("dummy", &DUMMY, "");
        init.start("dummy", &processes).unwrap();

        // The first restart is immediate.
        processes.exit(init.status_of("dummy").unwrap().pid.unwrap());
        assert_eq!(init.tick_at(&processes, 10), 1);
        assert_eq!(init.status_of("dummy").unwrap().restarts, 1);

        // The second waits, without looking like a running service.
        processes.exit(init.status_of("dummy").unwrap().pid.unwrap());
        assert_eq!(init.tick_at(&processes, 20), 1);
        let status = init.status_of("dummy").unwrap();
        assert_eq!(status.state, ServiceState::Stopped);
        assert_eq!(status.pid, None);
        assert_eq!(init.tick_at(&processes, 20 + RESTART_BACKOFF_MS - 1), 0);
        assert_eq!(processes.spawned(), 2);
        assert_eq!(init.tick_at(&processes, 20 + RESTART_BACKOFF_MS), 1);
        assert_eq!(init.status_of("dummy").unwrap().restarts, 2);

        // A healthy run refills the budget.
        init.tick_at(&processes, 20 + RESTART_BACKOFF_MS + HEALTHY_RUN_MS);
        assert_eq!(init.status_of("dummy").unwrap().restarts, 0);
    }

    #[test]
    fn manual_start_refills_a_spent_budget() {
        let processes = FakeProcesses::new();
        let mut init = Init::<4>::new(RestartPolicy::Always, 0);
        init.register("dummy", &DUMMY, "");
        let pid = init.start("dummy", &processes).unwrap();
        processes.exit(pid);
        init.tick(&processes);
        assert_eq!(init.status_of("dummy").unwrap().state, ServiceState::Failed);
        init.start("dummy", &processes).unwrap();
        assert_eq!(
            init.status_of("dummy").unwrap().state,
            ServiceState::Running
        );
    }

    #[test]
    fn full_process_table_is_retried_not_fatal() {
        let processes = FakeProcesses::new();
        let mut init = Init::<4>::new(RestartPolicy::Always, 3);
        init.register("dummy", &DUMMY, "");
        let pid = init.start("dummy", &processes).unwrap();
        processes.exit(pid);
        processes.state.lock().spawn_error = Some(SpawnError::NoFreeSlot);
        init.tick_at(&processes, 100);
        let status = init.status_of("dummy").unwrap();
        assert_eq!(status.state, ServiceState::Stopped);
        assert_eq!(status.restarts, 0);

        init.tick_at(&processes, 100 + SPAWN_RETRY_MS / 2);
        assert_eq!(init.status_of("dummy").unwrap().pid, None);
        init.tick_at(&processes, 100 + SPAWN_RETRY_MS);
        assert_eq!(
            init.status_of("dummy").unwrap().state,
            ServiceState::Stopped
        );

        processes.state.lock().spawn_error = None;
        init.tick_at(&processes, 100 + 2 * SPAWN_RETRY_MS);
        let status = init.status_of("dummy").unwrap();
        assert_eq!(status.state, ServiceState::Running);
        assert_eq!(status.restarts, 1);
    }

    #[test]
    fn permanent_spawn_error_fails_the_service() {
        let processes = FakeProcesses::new();
        let mut init = Init::<4>::new(RestartPolicy::Always, 3);
        init.register("dummy", &DUMMY, "");
        let pid = init.start("dummy", &processes).unwrap();
        processes.exit(pid);
        processes.state.lock().spawn_error = Some(SpawnError::ArgsTooLong);
        init.tick(&processes);
        assert_eq!(init.status_of("dummy").unwrap().state, ServiceState::Failed);
    }

    #[test]
    fn stopping_cancels_a_pending_restart() {
        let processes = FakeProcesses::new();
        let mut init = Init::<4>::new(RestartPolicy::Always, 3);
        init.register("dummy", &DUMMY, "");
        init.start("dummy", &processes).unwrap();
        processes.exit(init.status_of("dummy").unwrap().pid.unwrap());
        init.tick_at(&processes, 10);
        processes.exit(init.status_of("dummy").unwrap().pid.unwrap());
        init.tick_at(&processes, 20);
        init.stop("dummy", &processes);
        init.tick_at(&processes, 100_000);
        assert_eq!(init.status_of("dummy").unwrap().pid, None);
        assert_eq!(processes.spawned(), 2);
    }

    #[test]
    fn windowed_apps_are_not_spawned_or_restarted() {
        let processes = FakeProcesses::new();
        let mut init = Init::<4>::new(RestartPolicy::OnFailure, 3);
        assert!(init.register("terminal", &WINDOWED, ""));
        assert!(init.register("dummy", &DUMMY, ""));
        assert_eq!(init.start_all(&processes), 1);
        assert_eq!(processes.spawned(), 1);
        assert_eq!(init.start("terminal", &processes), Err(InitError::Windowed));
        init.tick(&processes);
        assert_eq!(
            init.status_of("terminal").unwrap().state,
            ServiceState::Stopped
        );
        let mut windowed = init.windowed_services();
        assert_eq!(windowed.next(), Some("terminal"));
        assert_eq!(windowed.next(), None);
    }

    #[test]
    fn oversized_ids_and_args_are_refused() {
        let mut init = Init::<4>::new(RestartPolicy::Never, 0);
        let long = "x".repeat(SERVICE_ID_MAX + 1);
        assert!(!init.register(&long, &DUMMY, ""));
        assert!(!init.register_missing(&long));
        assert!(!init.register("dummy", &DUMMY, &"a".repeat(SERVICE_ARGS_MAX + 1)));
        assert!(init.is_empty());
    }

    #[test]
    fn quoted_restart_policy_is_honoured() {
        let config = Config::parse("restart: \"always\"\n").unwrap();
        assert_eq!(config.policy, RestartPolicy::Always);
        let config = Config::parse("restart: 'never'\n").unwrap();
        assert_eq!(config.policy, RestartPolicy::Never);
    }

    #[test]
    fn large_documents_keep_their_init_section() {
        // Over the old 64-node arena, which silently reset `init:` to defaults
        // inside the desktop's 160-node configuration.
        let mut yaml = std::string::String::from("init:\n  restart: always\n");
        yaml.push_str("theme:\n");
        for index in 0..100 {
            yaml.push_str(&std::format!("  key{index}: {index}\n"));
        }
        let config = Config::parse(&yaml).unwrap();
        assert_eq!(config.policy, RestartPolicy::Always);

        let mut nodes = [YamlNode::EMPTY; 160];
        let document = YamlDocument::parse(&yaml, &mut nodes).unwrap();
        assert_eq!(Config::from_document(&document), Ok(config));
    }

    #[test]
    fn over_capacity_autostart_is_an_error() {
        let mut yaml = std::string::String::from("autostart:\n");
        for index in 0..AUTOSTART_MAX {
            yaml.push_str(&std::format!("  - s{index}\n"));
        }
        assert_eq!(Config::parse(&yaml).unwrap().autostart_len, AUTOSTART_MAX);
        yaml.push_str("  - one-too-many\n");
        assert_eq!(Config::try_parse(&yaml), Err(ConfigError::TooManyAutostart));
        assert_eq!(Config::parse(&yaml), Err(YamlError::BufferTooSmall));

        let long = std::format!("autostart:\n  - {}\n", "x".repeat(SERVICE_ID_MAX + 1));
        assert_eq!(
            Config::try_parse(&long),
            Err(ConfigError::InvalidAutostartId)
        );
    }

    #[test]
    fn missing_apps_are_reported() {
        let mut init = Init::<4>::new(RestartPolicy::Never, 0);
        assert!(init.register_missing("ghost"));
        assert_eq!(
            init.status_of("ghost").unwrap().state,
            ServiceState::Missing
        );
        let processes = FakeProcesses::new();
        assert_eq!(init.start("ghost", &processes), Err(InitError::UnknownApp));
    }
}
