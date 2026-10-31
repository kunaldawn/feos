//! A small init process manager and service supervisor.
//!
//! The kernel already provides preemptive processes; this crate adds the
//! policy layer a desktop session needs:
//!
//! - a list of autostart services read from YAML,
//! - name-to-app resolution through [`AppRegistry`],
//! - start, stop and restart by service id,
//! - restart policies with a bounded retry count,
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
use feos_yaml::{YamlDocument, YamlNode};

/// Default maximum number of supervised services.
pub const MAX_SERVICES: usize = 12;

/// Maximum length of a service id.
pub const SERVICE_ID_MAX: usize = 32;

/// Maximum length of service arguments.
pub const SERVICE_ARGS_MAX: usize = 64;

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
    pub autostart: [Text<SERVICE_ID_MAX>; 8],
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
            autostart: [Text::EMPTY; 8],
            autostart_len: 0,
            policy: RestartPolicy::OnFailure,
            max_restarts: 3,
        }
    }

    /// Parses the `init:` mapping from `yaml`.
    ///
    /// When the document has no `init:` key the root is used, so the same
    /// function accepts a standalone init file or the desktop configuration.
    pub fn parse(yaml: &str) -> Result<Self, feos_yaml::YamlError> {
        let mut nodes = [YamlNode::EMPTY; 64];
        let document = YamlDocument::parse(yaml, &mut nodes)?;
        let root = document.root();
        let section = document.get(root, "init").unwrap_or(root);
        let mut config = Self::new();

        if let Some(policy) = document
            .get(section, "restart")
            .and_then(|node| document.as_str_raw(node))
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
                if config.autostart_len == config.autostart.len() {
                    break;
                }
                let mut buffer = [0u8; 64];
                if let Some(value) = document.as_str(node, &mut buffer) {
                    let value = value.trim();
                    if !value.is_empty() {
                        config.autostart[config.autostart_len].set(value);
                        config.autostart_len += 1;
                    }
                }
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
}

impl Service {
    const EMPTY: Self = Self {
        id: Text::EMPTY,
        app: None,
        args: Text::EMPTY,
        pid: None,
        state: ServiceState::Stopped,
        restarts: 0,
    };
}

/// A fixed-capacity init/service manager.
pub struct Init<const N: usize = MAX_SERVICES> {
    services: [Service; N],
    len: usize,
    policy: RestartPolicy,
    max_restarts: u8,
}

impl<const N: usize> Init<N> {
    /// Creates an empty manager with the given restart policy.
    pub const fn new(policy: RestartPolicy, max_restarts: u8) -> Self {
        Self {
            services: [Service::EMPTY; N],
            len: 0,
            policy,
            max_restarts,
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
    /// Returns `false` when the id already exists or the table is full.
    pub fn register(&mut self, id: &str, app: &'static dyn App, args: &str) -> bool {
        if id.is_empty() || self.find(id).is_some() || self.len == N {
            return false;
        }
        let mut service = Service::EMPTY;
        service.id.set(id);
        service.app = Some(app);
        service.args.set(args);
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
        service.id.set(id);
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

    /// Starts a service.
    pub fn start(&mut self, id: &str, processes: &dyn Processes) -> Result<u32, InitError> {
        let index = self.find(id).ok_or(InitError::UnknownService)?;
        self.start_index(index, processes)
    }

    fn start_index(&mut self, index: usize, processes: &dyn Processes) -> Result<u32, InitError> {
        let service = &mut self.services[index];
        let app = service.app.ok_or(InitError::UnknownApp)?;
        let pid = processes
            .spawn(app, service.args.as_str())
            .map_err(InitError::Spawn)?;
        service.pid = Some(pid);
        service.state = ServiceState::Running;
        Ok(pid)
    }

    /// Starts every registered service that has an app.
    ///
    /// Returns the number of services that started successfully.
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
    pub fn stop(&mut self, id: &str, processes: &dyn Processes) -> bool {
        let Some(index) = self.find(id) else {
            return false;
        };
        self.stop_index(index, processes)
    }

    fn stop_index(&mut self, index: usize, processes: &dyn Processes) -> bool {
        let service = &mut self.services[index];
        let killed = match service.pid.take() {
            Some(pid) => processes.kill(pid),
            None => false,
        };
        service.state = if service.app.is_some() {
            ServiceState::Stopped
        } else {
            ServiceState::Missing
        };
        killed
    }

    /// Restarts a service, returning its new PID.
    pub fn restart(&mut self, id: &str, processes: &dyn Processes) -> Result<u32, InitError> {
        let index = self.find(id).ok_or(InitError::UnknownService)?;
        self.stop_index(index, processes);
        self.start_index(index, processes)
    }

    /// Checks every running service and applies the restart policy.
    ///
    /// Returns the number of services that were restarted or moved to
    /// `Failed`.
    pub fn tick(&mut self, processes: &dyn Processes) -> usize {
        if self.len == 0 {
            return 0;
        }

        let mut live = [ProcessInfo::EMPTY; MAX_PROCESSES];
        let count = processes.snapshot(&mut live);
        let mut changed = 0;

        for index in 0..self.len {
            let service = &mut self.services[index];
            let Some(pid) = service.pid else {
                continue;
            };
            if service.state != ServiceState::Running {
                continue;
            }
            if process_is_live(&live[..count], pid) {
                continue;
            }

            service.pid = None;
            changed += 1;

            if self.policy == RestartPolicy::Never {
                service.state = ServiceState::Stopped;
                continue;
            }
            if service.restarts >= self.max_restarts {
                service.state = ServiceState::Failed;
                continue;
            }

            let Some(app) = service.app else {
                service.state = ServiceState::Missing;
                continue;
            };
            match processes.spawn(app, service.args.as_str()) {
                Ok(new_pid) => {
                    service.pid = Some(new_pid);
                    service.restarts = service.restarts.saturating_add(1);
                    service.state = ServiceState::Running;
                }
                Err(_) => service.state = ServiceState::Failed,
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

        fn run(&self, _context: &mut Context<'_>) {}
    }

    static DUMMY: Dummy = Dummy;

    #[derive(Default)]
    struct FakeState {
        next_pid: u32,
        live: [u32; 8],
        live_len: usize,
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
                    name: [0; feos_app::PROCESS_NAME_MAX],
                    name_len: 0,
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
        assert!(init.register("missing", &DUMMY, ""));
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
