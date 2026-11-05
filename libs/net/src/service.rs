//! The blocking socket calls, around one [`Stack`].
//!
//! A task calling in takes the lock, tries what it wants, and if that has to wait,
//! sleeps until the network task says that something's happened. The network task
//! is the only one that sleeps on the device. See [`Network::service`].

use alloc::boxed::Box;
use alloc::string::ToString;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use feos_hal::net::{Endpoint, Ipv4Addr, NetBinding, NetConfig, NetDevice, NetError, NetStats, Network, SocketId, SocketInfo};
use feos_sync::{futex_wait_ms, futex_wake, Mutex};

use crate::dns;
use crate::stack::Stack;

/// The longest a waiter sleeps before looking at the clock and for a kill.
const WAIT_SLICE_MS: u64 = 50;
/// How often the network task looks for sockets whose owners have gone.
const SWEEP_MS: u64 = 1000;
/// Tries at a DNS question.
const DNS_TRIES: u64 = 3;

/// What the stack needs of the kernel.
#[derive(Clone, Copy)]
pub struct Hooks {
    /// Milliseconds since boot.
    pub now_ms: fn() -> u64,
    /// Microseconds since boot, as well as the clock can tell.
    pub now_us: fn() -> u64,
    /// The calling task.
    pub current: fn() -> u32,
    /// Whether a task exists.
    pub alive: fn(u32) -> bool,
    /// Whether the calling task has been asked to stop.
    pub kill_requested: fn() -> bool,
}

/// A device a board has leaked, as the stack wants to own one.
struct Leaked(&'static mut dyn NetDevice);

impl NetDevice for Leaked {
    fn mac(&self) -> [u8; 6] {
        self.0.mac()
    }
    fn link_up(&self) -> bool {
        self.0.link_up()
    }
    fn can_transmit(&mut self) -> bool {
        self.0.can_transmit()
    }
    fn transmit(&mut self, frame: &[u8]) -> bool {
        self.0.transmit(frame)
    }
    fn receive(&mut self, out: &mut [u8]) -> Option<usize> {
        self.0.receive(out)
    }
}

/// One stack, shared between the network task and everyone with a socket.
pub struct Service {
    stack: Mutex<Stack>,
    /// Moved by the device's interrupt, and to get the network task's attention.
    device_events: &'static AtomicU32,
    /// True when the device has an interrupt to wake the network task.
    interrupt_driven: bool,
    /// Moved when something happens that a waiter might want.
    events: AtomicU32,
    last_sweep: AtomicU64,
    hooks: Hooks,
}

impl Service {
    /// Wraps what a board found.
    pub fn new(binding: NetBinding, seed: u64, hooks: Hooks) -> Self {
        Self {
            stack: Mutex::new(Stack::new(Box::new(Leaked(binding.device)), seed)),
            device_events: binding.events,
            interrupt_driven: binding.how != "polled",
            events: AtomicU32::new(0),
            last_sweep: AtomicU64::new(0),
            hooks,
        }
    }

    /// Has the network task look again, because a timer may have changed.
    fn kick(&self) {
        self.device_events.fetch_add(1, Ordering::Release);
        futex_wake(self.device_events, usize::MAX);
    }

    fn now(&self) -> u64 {
        (self.hooks.now_ms)()
    }

    /// Tries `attempt` until it has an answer or `timeout_ms` is up.
    fn wait_for<T>(&self, timeout_ms: u64, mut attempt: impl FnMut(&mut Stack, u64) -> Result<Option<T>, NetError>) -> Result<T, NetError> {
        let deadline = self.now().saturating_add(timeout_ms);
        loop {
            // Before looking, so that news after the look stops the sleep.
            let seen = self.events.load(Ordering::Acquire);
            let now = self.now();
            let answer = {
                let mut stack = self.stack.lock();
                attempt(&mut stack, now)?
            };
            if let Some(value) = answer {
                return Ok(value);
            }
            if (self.hooks.kill_requested)() {
                return Err(NetError::Interrupted);
            }
            let now = self.now();
            if now >= deadline {
                return Err(NetError::TimedOut);
            }
            futex_wait_ms(&self.events, seen, (deadline - now).min(WAIT_SLICE_MS));
        }
    }

    fn ask_name_server(&self, name: &str, server: Ipv4Addr, timeout_ms: u64) -> Result<Ipv4Addr, NetError> {
        let (socket, _) = self.udp_bind(0)?;
        let result = (|| {
            let per_try = (timeout_ms / DNS_TRIES).max(1);
            for attempt in 0..DNS_TRIES {
                let id = (self.hooks.now_us)() as u16 ^ (socket.0 as u16).rotate_left(5) ^ attempt as u16;
                let query = dns::build_query(id, name).ok_or(NetError::Invalid)?;
                self.udp_send_to(socket, Endpoint { addr: server, port: dns::PORT }, &query)?;
                let deadline = self.now().saturating_add(per_try);
                loop {
                    let left = deadline.saturating_sub(self.now());
                    if left == 0 {
                        break;
                    }
                    let mut reply = [0u8; 1500];
                    let (length, from) = match self.udp_recv_from(socket, &mut reply, left) {
                        Ok(got) => got,
                        Err(NetError::TimedOut) => break,
                        Err(error) => return Err(error),
                    };
                    if from.addr != server || from.port != dns::PORT {
                        continue;
                    }
                    match dns::parse_response(&reply[..length], id, name) {
                        Some(dns::Answer::Address(addr)) => return Ok(addr),
                        Some(_) => return Err(NetError::NotFound),
                        None => {}
                    }
                }
            }
            Err(NetError::TimedOut)
        })();
        self.close(socket);
        result
    }
}

impl Network for Service {
    fn config(&self) -> NetConfig {
        self.stack.lock().config(self.now())
    }

    fn stats(&self) -> NetStats {
        self.stack.lock().stats()
    }

    fn sockets(&self, out: &mut [SocketInfo]) -> usize {
        self.stack.lock().sockets(out)
    }

    fn wait_configured(&self, timeout_ms: u64) -> bool {
        self.wait_for(timeout_ms, |stack, _| Ok(stack.is_configured().then_some(()))).is_ok()
    }

    fn service(&self, max_wait_ms: u64) {
        let seen = self.device_events.load(Ordering::Acquire);
        let now = self.now();
        let result = {
            let mut stack = self.stack.lock();
            let result = stack.poll(now);
            if now.saturating_sub(self.last_sweep.load(Ordering::Relaxed)) >= SWEEP_MS {
                self.last_sweep.store(now, Ordering::Relaxed);
                stack.close_orphans(self.hooks.alive, now);
            }
            result
        };
        if result.activity {
            self.events.fetch_add(1, Ordering::Release);
            futex_wake(&self.events, usize::MAX);
        }
        if result.more {
            return;
        }
        let wait = result
            .next_deadline
            .map_or(max_wait_ms, |deadline| deadline.saturating_sub(now))
            .clamp(1, max_wait_ms.max(1));
        // Without an interrupt, the only news of an arrival is looking for it.
        let wait = if self.interrupt_driven { wait } else { wait.min(2) };
        futex_wait_ms(self.device_events, seen, wait);
    }

    fn ping(&self, target: Ipv4Addr, sequence: u16, payload: usize, timeout_ms: u64) -> Result<u64, NetError> {
        let sent_at = (self.hooks.now_us)();
        let id = {
            let mut stack = self.stack.lock();
            stack.ping_start(target, sequence, payload, self.now())?
        };
        self.kick();
        let result = self.wait_for(timeout_ms, |stack, _| Ok(stack.ping_answered(id).then_some(())));
        self.stack.lock().ping_finish(id);
        result.map(|()| (self.hooks.now_us)().saturating_sub(sent_at))
    }

    fn resolve(&self, name: &str, timeout_ms: u64) -> Result<Ipv4Addr, NetError> {
        if let Some(addr) = Ipv4Addr::parse(name) {
            return Ok(addr);
        }
        if !self.wait_configured(timeout_ms) {
            return Err(NetError::NotConfigured);
        }
        let Some(server) = self.stack.lock().name_server() else {
            return Err(NetError::NotFound);
        };
        // A copy with no dot on the end is what's compared with the answer.
        let name = name.strip_suffix('.').unwrap_or(name).to_string();
        self.ask_name_server(&name, server, timeout_ms)
    }

    fn tcp_connect(&self, remote: Endpoint, timeout_ms: u64) -> Result<SocketId, NetError> {
        let id = {
            let mut stack = self.stack.lock();
            stack.tcp_connect(remote, (self.hooks.current)(), self.now())?
        };
        self.kick();
        let up = self.wait_for(timeout_ms, |stack, _| Ok(stack.tcp_connected(id)?.then_some(())));
        if up.is_err() {
            self.close(id);
        }
        up.map(|()| id)
    }

    fn tcp_listen(&self, port: u16, backlog: usize) -> Result<SocketId, NetError> {
        self.stack.lock().tcp_listen(port, backlog, (self.hooks.current)())
    }

    fn tcp_accept(&self, listener: SocketId, timeout_ms: u64) -> Result<(SocketId, Endpoint), NetError> {
        let owner = (self.hooks.current)();
        self.wait_for(timeout_ms, |stack, _| stack.tcp_accept(listener, owner))
    }

    fn tcp_send(&self, socket: SocketId, data: &[u8], timeout_ms: u64) -> Result<usize, NetError> {
        let count = self.wait_for(timeout_ms, |stack, now| stack.tcp_send(socket, data, now))?;
        self.kick();
        Ok(count)
    }

    fn tcp_recv(&self, socket: SocketId, out: &mut [u8], timeout_ms: u64) -> Result<usize, NetError> {
        self.wait_for(timeout_ms, |stack, now| stack.tcp_recv(socket, out, now))
    }

    fn tcp_shutdown(&self, socket: SocketId) -> Result<(), NetError> {
        let result = self.stack.lock().tcp_shutdown(socket, self.now());
        self.kick();
        result
    }

    fn tcp_flush(&self, socket: SocketId, timeout_ms: u64) -> Result<(), NetError> {
        self.wait_for(timeout_ms, |stack, _| Ok(stack.tcp_flushed(socket)?.then_some(())))
    }

    fn udp_bind(&self, port: u16) -> Result<(SocketId, u16), NetError> {
        self.stack.lock().udp_bind(port, (self.hooks.current)())
    }

    fn udp_send_to(&self, socket: SocketId, to: Endpoint, data: &[u8]) -> Result<(), NetError> {
        let result = self.stack.lock().udp_send_to(socket, to, data, self.now());
        self.kick();
        result
    }

    fn udp_recv_from(&self, socket: SocketId, out: &mut [u8], timeout_ms: u64) -> Result<(usize, Endpoint), NetError> {
        self.wait_for(timeout_ms, |stack, _| stack.udp_recv_from(socket, out))
    }

    fn close(&self, socket: SocketId) {
        self.stack.lock().close(socket, self.now());
        self.kick();
    }
}
