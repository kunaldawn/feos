//! `netd`: runs the TCP/IP stack, so that nothing else has to.
//!
//! The stack only moves when something calls into it. This task takes in what the card
//! has received and sends what's due, sleeping on the card's interrupt in between, so
//! the desktop and everyone else keep to their own business.

use core::sync::atomic::{AtomicU64, Ordering};

use feos_app::{App, Context, Platform, SpawnIo};
use feos_console::println;
use feos_hal::net::{Ipv4Addr, NetConfig};

/// The longest it sleeps with nothing to do.
const MAX_WAIT_MS: u64 = 250;
/// Silent for this long, it's not running: someone killed it.
const STALE_AFTER_MS: u64 = 5000;
/// Urgent, not greedy: it only runs when there's a packet to deal with.
const PRIORITY: u8 = 11;

static HEARTBEAT_MS: AtomicU64 = AtomicU64::new(u64::MAX);

/// The network task.
pub struct NetDaemon;

static DAEMON: NetDaemon = NetDaemon;

fn prefix(mask: Ipv4Addr) -> u32 {
    mask.to_u32().leading_ones()
}

fn report_change(before: &NetConfig, now: &NetConfig) {
    if before.addr != now.addr {
        if now.addr.is_unspecified() {
            println!("[net] lease lost");
        } else {
            println!(
                "[net] lease {}/{} gateway {} dns {} from {}",
                now.addr,
                prefix(now.netmask),
                now.gateway,
                now.dns,
                now.dhcp_server
            );
        }
    }
}

impl App for NetDaemon {
    fn name(&self) -> &'static str {
        "netd"
    }

    fn usage(&self) -> &'static str {
        "netd"
    }

    fn description(&self) -> &'static str {
        "run the TCP/IP stack"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        let platform = context.platform;
        let Some(net) = platform.net else {
            return 0;
        };
        let mut known = net.config();
        loop {
            HEARTBEAT_MS.store(platform.clock.uptime_ms(), Ordering::Release);
            net.service(MAX_WAIT_MS);
            let config = net.config();
            report_change(&known, &config);
            known = config;
            if platform.processes.kill_requested() {
                HEARTBEAT_MS.store(u64::MAX, Ordering::Release);
                return feos_app::EXIT_KILLED;
            }
        }
    }
}

/// Starts the task, on a machine with a network card.
pub fn install(platform: &'static Platform) {
    if platform.net.is_none() {
        return;
    }
    HEARTBEAT_MS.store(platform.clock.uptime_ms(), Ordering::Release);
    let io = SpawnIo {
        background: true,
        ..SpawnIo::default()
    };
    if let Ok(pid) = platform.processes.spawn_argv(&DAEMON, &[], io) {
        platform.processes.set_priority(pid, PRIORITY);
    }
}

/// Starts the task again if someone has killed it.
pub fn ensure_running(platform: &'static Platform) {
    let beat = HEARTBEAT_MS.load(Ordering::Acquire);
    if beat == u64::MAX || platform.clock.uptime_ms().saturating_sub(beat) > STALE_AFTER_MS {
        install(platform);
    }
}
