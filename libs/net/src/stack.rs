//! The stack proper: one interface, its addresses, and all its sockets.
//!
//! Nothing here blocks or knows what time it is: every entry point is told. Calls
//! that would have to wait answer `Ok(None)`, and [`crate::service`] does the waiting.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;

use feos_hal::net::{Endpoint, Ipv4Addr, NetConfig, NetDevice, NetError, NetStats, SocketId, SocketInfo};

use crate::dhcp::{self, Change, Client};
use crate::tcp::{self, Incoming, Outgoing, Tcb};
use crate::wire::{self, Arp, ETHERTYPE_ARP, ETHERTYPE_IPV4, ETH_HEADER, IP_HEADER, MAC_BROADCAST, MAX_FRAME, PROTO_ICMP, PROTO_TCP, PROTO_UDP, TCP_ACK, TCP_RST, TCP_SYN, UDP_HEADER};

/// Sockets at once, of all kinds.
pub const MAX_SOCKETS: usize = 64;
/// Neighbours remembered.
const ARP_ENTRIES: usize = 32;
/// How long a neighbour's address is believed.
const ARP_TTL_MS: u64 = 300_000;
/// Between requests for the same neighbour.
const ARP_RETRY_MS: u64 = 500;
/// Packets waiting for a neighbour's address, at most.
const MAX_PENDING: usize = 16;
/// How long one waits.
const PENDING_MS: u64 = 3000;
/// Datagrams a socket holds before dropping the next.
const UDP_QUEUE: usize = 64;
/// Bytes a socket holds before dropping the next.
const UDP_QUEUE_BYTES: usize = 256 * 1024;
/// Largest datagram: an unfragmented one.
pub const MAX_DATAGRAM: usize = wire::MTU - IP_HEADER - UDP_HEADER;
/// Echo requests in the air at once.
const MAX_PINGS: usize = 16;
/// First of the ports handed out, the dynamic range of RFC 6335.
const EPHEMERAL_FIRST: u16 = 49152;
/// How long a closed connection waits for a neighbour that doesn't answer.
const STRANDED_MS: u64 = 60_000;
/// Frames to take in one pass, so that a flood can't hold the lock for ever.
const RX_BUDGET: usize = 64;

/// Whether the address is this subnet's, a point everyone listens to.
fn is_broadcast(addr: Ipv4Addr, own: Ipv4Addr, mask: Ipv4Addr) -> bool {
    addr == Ipv4Addr::BROADCAST || (mask != Ipv4Addr::BROADCAST && addr.to_u32() == (own.to_u32() | !mask.to_u32()))
}

struct ArpEntry {
    ip: Ipv4Addr,
    mac: Option<[u8; 6]>,
    /// When a known address is stale, or when a request can go out again.
    until: u64,
}

struct Pending {
    next_hop: Ipv4Addr,
    frame: Vec<u8>,
    expires: u64,
}

struct TcpSocket {
    local_port: u16,
    remote: Endpoint,
    tcb: Box<Tcb>,
    /// The listener that hasn't heard of this yet.
    parent: Option<SocketId>,
    /// The owner has closed it. It lingers until the connection's over.
    closing: bool,
    /// Since when it's had something to send and no neighbour to send it to. TCP's
    /// timers only run on what's gone out, so they don't cover this.
    stranded_since: Option<u64>,
}

struct Listener {
    port: u16,
    backlog: usize,
    /// Established, and waiting for `accept`.
    ready: VecDeque<SocketId>,
}

struct UdpSocket {
    port: u16,
    queue: VecDeque<(Endpoint, Vec<u8>)>,
    queued_bytes: usize,
}

enum Kind {
    Tcp(TcpSocket),
    Listener(Listener),
    Udp(UdpSocket),
}

struct Socket {
    owner: u32,
    kind: Kind,
}

#[derive(Default)]
struct Slot {
    generation: u16,
    socket: Option<Socket>,
}

struct Ping {
    target: Ipv4Addr,
    id: u16,
    sequence: u16,
    answered: bool,
}

/// What a pass of [`Stack::poll`] came to.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PollResult {
    /// Something arrived or changed, and a waiting task should look again.
    pub activity: bool,
    /// More frames may be waiting: go again without sleeping.
    pub more: bool,
    /// When this next needs to run, with nothing arriving.
    pub next_deadline: Option<u64>,
}

/// One interface and its sockets.
pub struct Stack {
    device: Box<dyn NetDevice>,
    mac: [u8; 6],
    dhcp: Client,
    addr: Ipv4Addr,
    netmask: Ipv4Addr,
    gateway: Ipv4Addr,
    dns: Ipv4Addr,
    server: Ipv4Addr,
    lease_expires: u64,
    arp: Vec<ArpEntry>,
    pending: Vec<Pending>,
    slots: Vec<Slot>,
    pings: Vec<Ping>,
    random: u64,
    ip_id: u16,
    stats: NetStats,
    /// A socket's state moved in a way a waiter should see.
    changed: bool,
}

impl Stack {
    /// A stack over `device`. `seed` starts its random numbers: sequence numbers,
    /// ports and DNS IDs. It isn't a secure source, so an attacker who can watch the
    /// traffic can guess them: see `docs/networking.md`.
    pub fn new(device: Box<dyn NetDevice>, seed: u64) -> Self {
        let mac = device.mac();
        let mut random = seed ^ 0x9e37_79b9_7f4a_7c15;
        for byte in mac {
            random = random.rotate_left(8) ^ u64::from(byte);
        }
        let mut stack = Self {
            device,
            mac,
            dhcp: Client::new(mac, 0),
            addr: Ipv4Addr::UNSPECIFIED,
            netmask: Ipv4Addr::UNSPECIFIED,
            gateway: Ipv4Addr::UNSPECIFIED,
            dns: Ipv4Addr::UNSPECIFIED,
            server: Ipv4Addr::UNSPECIFIED,
            lease_expires: 0,
            arp: Vec::new(),
            pending: Vec::new(),
            slots: Vec::new(),
            pings: Vec::new(),
            random: random | 1,
            ip_id: 0,
            stats: NetStats::default(),
            changed: false,
        };
        stack.dhcp = Client::new(mac, stack.next_random() as u32);
        stack.ip_id = stack.next_random() as u16;
        stack
    }

    fn next_random(&mut self) -> u64 {
        // xorshift64*.
        self.random ^= self.random >> 12;
        self.random ^= self.random << 25;
        self.random ^= self.random >> 27;
        self.random.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// How the interface is set up.
    pub fn config(&self, now: u64) -> NetConfig {
        NetConfig {
            mac: self.mac,
            link_up: self.device.link_up(),
            addr: self.addr,
            netmask: self.netmask,
            gateway: self.gateway,
            dns: self.dns,
            dhcp_server: self.server,
            lease_remaining_ms: self.lease_expires.saturating_sub(now),
        }
    }

    /// Counters since boot, and how many sockets there are.
    pub fn stats(&self) -> NetStats {
        let mut stats = self.stats;
        for slot in &self.slots {
            match slot.socket.as_ref().map(|socket| &socket.kind) {
                Some(Kind::Tcp(_) | Kind::Listener(_)) => stats.tcp_sockets += 1,
                Some(Kind::Udp(_)) => stats.udp_sockets += 1,
                None => {}
            }
        }
        stats
    }

    /// Lists the open sockets.
    pub fn sockets(&self, out: &mut [SocketInfo]) -> usize {
        let mut count = 0;
        for slot in &self.slots {
            let Some(socket) = slot.socket.as_ref() else {
                continue;
            };
            let Some(entry) = out.get_mut(count) else {
                break;
            };
            *entry = match &socket.kind {
                Kind::Tcp(tcp) => SocketInfo {
                    state: tcp.tcb.state().name(),
                    local: Endpoint { addr: self.addr, port: tcp.local_port },
                    remote: tcp.remote,
                    owner: socket.owner,
                },
                Kind::Listener(listener) => SocketInfo {
                    state: "LISTEN",
                    local: Endpoint { addr: self.addr, port: listener.port },
                    remote: Endpoint::default(),
                    owner: socket.owner,
                },
                Kind::Udp(udp) => SocketInfo {
                    state: "udp",
                    local: Endpoint { addr: self.addr, port: udp.port },
                    remote: Endpoint::default(),
                    owner: socket.owner,
                },
            };
            count += 1;
        }
        count
    }

    // ---- The passage of time, and whatever's come in. ----

    /// Takes in what's arrived, sends what's due, and says when to come back.
    pub fn poll(&mut self, now: u64) -> PollResult {
        let mut result = PollResult::default();
        let mut frame = [0u8; MAX_FRAME + 64];
        let mut taken = 0;
        while taken < RX_BUDGET {
            let Some(length) = self.device.receive(&mut frame) else {
                break;
            };
            taken += 1;
            self.stats.rx_frames += 1;
            self.stats.rx_bytes += length as u64;
            if let Some(data) = frame.get(..length) {
                self.input(data, now);
            }
        }
        result.more = taken == RX_BUDGET;
        result.activity = taken != 0;

        self.poll_dhcp(now);
        self.retry_pending(now);

        for index in 0..self.slots.len() {
            self.service_slot(index, now);
        }
        result.activity |= core::mem::take(&mut self.changed);
        result.next_deadline = self.next_deadline(now);
        result
    }

    fn next_deadline(&mut self, now: u64) -> Option<u64> {
        let mut deadline = self.dhcp.next_deadline();
        let waiting_on_arp = !self.pending.is_empty();
        for slot in &self.slots {
            if let Some(Socket { kind: Kind::Tcp(tcp), .. }) = &slot.socket {
                if let Some(next) = tcp.tcb.next_deadline() {
                    deadline = deadline.min(next);
                }
                // Held back by a full device, or an unknown neighbour, which takes longer.
                if tcp.tcb.wants_to_send() {
                    let wait = if tcp.stranded_since.is_some() { ARP_RETRY_MS } else { 1 };
                    deadline = deadline.min(now + wait);
                }
            }
        }
        if waiting_on_arp {
            deadline = deadline.min(now + ARP_RETRY_MS);
        }
        Some(deadline)
    }

    fn poll_dhcp(&mut self, now: u64) {
        let (send, change) = self.dhcp.poll(now);
        self.apply_dhcp(change, now);
        if let Some(send) = send {
            self.send_udp_from(send.from, dhcp::CLIENT_PORT, Endpoint { addr: send.to, port: dhcp::SERVER_PORT }, &send.payload, now);
        }
    }

    fn apply_dhcp(&mut self, change: Change, _now: u64) {
        match change {
            Change::None => {}
            Change::Leased => {
                if let Some(lease) = self.dhcp.lease().copied() {
                    self.addr = lease.addr;
                    self.netmask = lease.netmask;
                    self.gateway = lease.router;
                    self.dns = lease.dns;
                    self.server = lease.server;
                    self.lease_expires = lease.obtained_ms() + lease.remaining_ms(lease.obtained_ms());
                    self.changed = true;
                }
            }
            Change::Lost => {
                self.addr = Ipv4Addr::UNSPECIFIED;
                self.netmask = Ipv4Addr::UNSPECIFIED;
                self.gateway = Ipv4Addr::UNSPECIFIED;
                self.dns = Ipv4Addr::UNSPECIFIED;
                self.server = Ipv4Addr::UNSPECIFIED;
                self.lease_expires = 0;
                self.changed = true;
            }
        }
    }

    /// Runs one socket's timers and sends what it has, and frees it if it's over.
    fn service_slot(&mut self, index: usize, now: u64) {
        let Some(Socket { kind: Kind::Tcp(tcp), .. }) = self.slots.get_mut(index).and_then(|slot| slot.socket.as_mut())
        else {
            return;
        };
        let was_closed = tcp.tcb.is_closed();
        tcp.tcb.on_timer(now);
        if tcp.tcb.is_closed() != was_closed {
            self.changed = true;
        }
        self.flush_tcp(index, now);
    }

    /// True if this socket has reached its end and nobody will ask about it.
    fn reap_if_done(&mut self, index: usize) {
        let Some(slot) = self.slots.get_mut(index) else {
            return;
        };
        let done = match slot.socket.as_ref().map(|socket| &socket.kind) {
            Some(Kind::Tcp(tcp)) => {
                tcp.tcb.is_closed() && !tcp.tcb.wants_to_send() && (tcp.closing || tcp.parent.is_some())
            }
            _ => false,
        };
        if done {
            slot.socket = None;
            slot.generation = slot.generation.wrapping_add(1);
        }
    }

    /// Sends everything this connection has ready.
    fn flush_tcp(&mut self, index: usize, now: u64) {
        let mut frame = [0u8; MAX_FRAME];
        loop {
            let Some(Socket { kind: Kind::Tcp(tcp), .. }) = self.slots.get(index).and_then(|slot| slot.socket.as_ref())
            else {
                return;
            };
            let (local_port, remote) = (tcp.local_port, tcp.remote);
            if !tcp.tcb.wants_to_send() {
                break;
            }
            if !self.device.can_transmit() {
                break;
            }
            let neighbour = self.neighbour(remote.addr, now);
            let Some(Socket { kind: Kind::Tcp(tcp), .. }) = self.slots.get_mut(index).and_then(|slot| slot.socket.as_mut())
            else {
                return;
            };
            let Some(mac) = neighbour else {
                let since = *tcp.stranded_since.get_or_insert(now);
                // With no owner left, nobody's going to give up on it otherwise.
                if tcp.closing && now.saturating_sub(since) >= STRANDED_MS {
                    tcp.tcb.abort();
                    tcp.tcb.next_segment(now, &mut frame);
                }
                break;
            };
            tcp.stranded_since = None;
            let at = ETH_HEADER + IP_HEADER + wire::TCP_HEADER;
            let Some(segment) = tcp.tcb.next_segment(now, &mut frame[at..]) else {
                break;
            };
            self.stats.tcp_retransmits += tcp.tcb.take_retransmits();
            self.send_segment(&mut frame, mac, local_port, remote, &segment);
        }
        // Tell a listener about a connection that's now ready.
        self.promote_if_ready(index);
        self.reap_if_done(index);
    }

    fn promote_if_ready(&mut self, index: usize) {
        let Some(slot) = self.slots.get_mut(index) else {
            return;
        };
        let generation = slot.generation;
        let Some(Socket { kind: Kind::Tcp(tcp), .. }) = slot.socket.as_mut() else {
            return;
        };
        if !tcp.tcb.is_connected() {
            return;
        }
        let Some(parent) = tcp.parent.take() else {
            return;
        };
        let id = make_id(index, generation);
        self.changed = true;
        match self.listener_mut(parent) {
            Some(listener) => listener.ready.push_back(id),
            None => {
                // The listener went while this was on its way in.
                if let Some(Socket { kind: Kind::Tcp(tcp), .. }) =
                    self.slots.get_mut(index).and_then(|slot| slot.socket.as_mut())
                {
                    tcp.tcb.abort();
                    tcp.closing = true;
                }
            }
        }
    }

    fn listener_mut(&mut self, id: SocketId) -> Option<&mut Listener> {
        match self.socket_mut(id)?.kind {
            Kind::Listener(ref mut listener) => Some(listener),
            _ => None,
        }
    }

    fn send_segment(&mut self, frame: &mut [u8], mac: [u8; 6], local_port: u16, remote: Endpoint, segment: &Outgoing) {
        let header = wire::TcpHeader {
            src_port: local_port,
            dst_port: remote.port,
            seq: segment.seq,
            ack: segment.ack,
            flags: segment.flags,
            window: segment.window,
            mss: segment.mss,
        };
        let length = wire::write_tcp(&mut frame[ETH_HEADER + IP_HEADER..], self.addr, remote.addr, &header, segment.len);
        self.ip_id = self.ip_id.wrapping_add(1);
        wire::write_ipv4(&mut frame[ETH_HEADER..], self.addr, remote.addr, PROTO_TCP, length, self.ip_id);
        wire::write_ethernet(frame, mac, self.mac, ETHERTYPE_IPV4);
        self.transmit(&frame[..ETH_HEADER + IP_HEADER + length]);
    }

    fn transmit(&mut self, frame: &[u8]) {
        if self.device.transmit(frame) {
            self.stats.tx_frames += 1;
            self.stats.tx_bytes += frame.len() as u64;
        } else {
            self.stats.tx_dropped += 1;
        }
    }

    // ---- Neighbours. ----

    /// Where a packet to `dst` goes first.
    fn next_hop(&self, dst: Ipv4Addr) -> Ipv4Addr {
        let mask = self.netmask.to_u32();
        if (dst.to_u32() ^ self.addr.to_u32()) & mask == 0 || self.gateway.is_unspecified() {
            dst
        } else {
            self.gateway
        }
    }

    /// A destination's hardware address, asking for it if it isn't known.
    fn neighbour(&mut self, dst: Ipv4Addr, now: u64) -> Option<[u8; 6]> {
        if is_broadcast(dst, self.addr, self.netmask) {
            return Some(MAC_BROADCAST);
        }
        let hop = self.next_hop(dst);
        if let Some(at) = self.arp.iter().position(|entry| entry.ip == hop) {
            let entry = &self.arp[at];
            if let Some(mac) = entry.mac {
                if now < entry.until {
                    return Some(mac);
                }
            }
            // Stale, or asked about and not yet answered.
            if now >= entry.until {
                self.arp[at].mac = None;
                self.arp[at].until = now + ARP_RETRY_MS;
                self.send_arp_request(hop);
            }
            return None;
        }
        if self.arp.len() >= ARP_ENTRIES {
            // The one that goes stale first.
            let oldest = (0..self.arp.len()).min_by_key(|&index| self.arp[index].until)?;
            self.arp.swap_remove(oldest);
        }
        self.arp.push(ArpEntry {
            ip: hop,
            mac: None,
            until: now + ARP_RETRY_MS,
        });
        self.send_arp_request(hop);
        None
    }

    fn send_arp_request(&mut self, ip: Ipv4Addr) {
        let packet = Arp {
            operation: wire::ARP_REQUEST,
            sender_mac: self.mac,
            sender_ip: self.addr,
            target_mac: [0; 6],
            target_ip: ip,
        };
        self.send_arp(MAC_BROADCAST, &packet);
    }

    fn send_arp(&mut self, dst: [u8; 6], packet: &Arp) {
        let mut frame = [0u8; ETH_HEADER + wire::ARP_LEN];
        wire::write_ethernet(&mut frame, dst, self.mac, ETHERTYPE_ARP);
        wire::write_arp(&mut frame[ETH_HEADER..], packet);
        self.transmit(&frame);
    }

    fn learn(&mut self, ip: Ipv4Addr, mac: [u8; 6], now: u64) {
        if let Some(entry) = self.arp.iter_mut().find(|entry| entry.ip == ip) {
            entry.mac = Some(mac);
            entry.until = now + ARP_TTL_MS;
        } else if self.arp.len() < ARP_ENTRIES {
            self.arp.push(ArpEntry {
                ip,
                mac: Some(mac),
                until: now + ARP_TTL_MS,
            });
        }
        // Whatever was waiting for this one can go.
        let mut index = 0;
        while index < self.pending.len() {
            if self.pending[index].next_hop == ip {
                let mut waiting = self.pending.swap_remove(index);
                if let Some(head) = waiting.frame.get_mut(..6) {
                    head.copy_from_slice(&mac);
                }
                self.transmit(&waiting.frame);
            } else {
                index += 1;
            }
        }
    }

    fn retry_pending(&mut self, now: u64) {
        self.pending.retain(|waiting| now < waiting.expires);
        let mut index = 0;
        while index < self.pending.len() {
            let hop = self.pending[index].next_hop;
            // Asks again as needed. Sends nothing when it's within the retry time.
            if let Some(mac) = self.neighbour(hop, now) {
                let mut waiting = self.pending.swap_remove(index);
                if let Some(head) = waiting.frame.get_mut(..6) {
                    head.copy_from_slice(&mac);
                }
                self.transmit(&waiting.frame);
            } else {
                index += 1;
            }
        }
    }

    /// Sends a frame once its destination's address is known.
    fn send_or_hold(&mut self, frame: &mut [u8], dst: Ipv4Addr, now: u64) -> bool {
        match self.neighbour(dst, now) {
            Some(mac) => {
                if let Some(head) = frame.get_mut(..6) {
                    head.copy_from_slice(&mac);
                }
                self.transmit(frame);
                true
            }
            None => {
                if self.pending.len() >= MAX_PENDING {
                    self.stats.tx_dropped += 1;
                    return false;
                }
                self.pending.push(Pending {
                    next_hop: self.next_hop(dst),
                    frame: frame.to_vec(),
                    expires: now + PENDING_MS,
                });
                true
            }
        }
    }

    // ---- What's come in. ----

    /// Takes one Ethernet frame.
    pub fn input(&mut self, frame: &[u8], now: u64) {
        let Some(ethernet) = wire::parse_ethernet(frame) else {
            self.stats.rx_dropped += 1;
            return;
        };
        if ethernet.dst != self.mac && ethernet.dst != MAC_BROADCAST {
            self.stats.rx_dropped += 1;
            return;
        }
        match ethernet.ethertype {
            ETHERTYPE_ARP => self.input_arp(ethernet.payload, now),
            ETHERTYPE_IPV4 => self.input_ipv4(ethernet.payload, now),
            _ => self.stats.rx_dropped += 1,
        }
    }

    fn input_arp(&mut self, data: &[u8], now: u64) {
        let Some(packet) = wire::parse_arp(data) else {
            self.stats.rx_dropped += 1;
            return;
        };
        // Only what's addressed to this host, so that nobody's chatter fills the
        // table. What's in it can be overwritten by anyone on the link: ARP has no
        // way to tell.
        if self.addr.is_unspecified() || packet.target_ip != self.addr {
            return;
        }
        if packet.sender_ip.is_unspecified() || packet.sender_mac == MAC_BROADCAST {
            return;
        }
        self.learn(packet.sender_ip, packet.sender_mac, now);
        if packet.operation == wire::ARP_REQUEST {
            let reply = Arp {
                operation: wire::ARP_REPLY,
                sender_mac: self.mac,
                sender_ip: self.addr,
                target_mac: packet.sender_mac,
                target_ip: packet.sender_ip,
            };
            self.send_arp(packet.sender_mac, &reply);
        }
    }

    fn input_ipv4(&mut self, data: &[u8], now: u64) {
        let Some(ip) = wire::parse_ipv4(data) else {
            self.stats.rx_dropped += 1;
            return;
        };
        // DHCP's messages come to an address this host doesn't have yet.
        let dhcp = !self.dhcp_bound() && ip.protocol == PROTO_UDP;
        let for_us = ip.dst == self.addr || is_broadcast(ip.dst, self.addr, self.netmask);
        if !(for_us || dhcp) || (self.addr.is_unspecified() && !dhcp) {
            self.stats.rx_dropped += 1;
            return;
        }
        let broadcast = ip.dst != self.addr;
        match ip.protocol {
            PROTO_ICMP if !broadcast => self.input_icmp(ip.src, ip.payload, now),
            PROTO_UDP => self.input_udp(ip.src, ip.dst, ip.payload, now),
            PROTO_TCP if !broadcast => self.input_tcp(ip.src, ip.dst, ip.payload, now),
            _ => self.stats.rx_dropped += 1,
        }
    }

    fn dhcp_bound(&self) -> bool {
        self.dhcp.lease().is_some()
    }

    fn input_icmp(&mut self, src: Ipv4Addr, data: &[u8], now: u64) {
        let Some(echo) = wire::parse_icmp_echo(data) else {
            self.stats.rx_dropped += 1;
            return;
        };
        if echo.kind == wire::ICMP_ECHO_REPLY {
            for ping in &mut self.pings {
                if ping.target == src && ping.id == echo.id && ping.sequence == echo.sequence {
                    ping.answered = true;
                    self.changed = true;
                }
            }
            return;
        }
        let mut frame = [0u8; MAX_FRAME];
        if ICMP_LIMIT < echo.data.len() {
            return;
        }
        let length = wire::write_icmp_echo(&mut frame[ETH_HEADER + IP_HEADER..], wire::ICMP_ECHO_REPLY, echo.id, echo.sequence, echo.data);
        self.send_ip(&mut frame, src, PROTO_ICMP, length, now);
    }

    fn send_ip(&mut self, frame: &mut [u8], dst: Ipv4Addr, protocol: u8, payload: usize, now: u64) -> bool {
        self.send_ip_from(frame, self.addr, dst, protocol, payload, now)
    }

    fn send_ip_from(&mut self, frame: &mut [u8], src: Ipv4Addr, dst: Ipv4Addr, protocol: u8, payload: usize, now: u64) -> bool {
        self.ip_id = self.ip_id.wrapping_add(1);
        wire::write_ipv4(&mut frame[ETH_HEADER..], src, dst, protocol, payload, self.ip_id);
        wire::write_ethernet(frame, MAC_BROADCAST, self.mac, ETHERTYPE_IPV4);
        self.send_or_hold(&mut frame[..ETH_HEADER + IP_HEADER + payload], dst, now)
    }

    fn input_udp(&mut self, src: Ipv4Addr, dst: Ipv4Addr, data: &[u8], now: u64) {
        let Some(udp) = wire::parse_udp(src, dst, data) else {
            self.stats.rx_dropped += 1;
            return;
        };
        if udp.dst_port == dhcp::CLIENT_PORT && udp.src_port == dhcp::SERVER_PORT {
            let change = self.dhcp.on_message(udp.payload, now);
            self.apply_dhcp(change, now);
            return;
        }
        if self.addr.is_unspecified() {
            self.stats.rx_dropped += 1;
            return;
        }
        for slot in &mut self.slots {
            let Some(Socket { kind: Kind::Udp(socket), .. }) = slot.socket.as_mut() else {
                continue;
            };
            if socket.port == udp.dst_port {
                if socket.queue.len() < UDP_QUEUE && socket.queued_bytes + udp.payload.len() <= UDP_QUEUE_BYTES {
                    socket.queued_bytes += udp.payload.len();
                    socket.queue.push_back((Endpoint { addr: src, port: udp.src_port }, udp.payload.to_vec()));
                    self.changed = true;
                } else {
                    self.stats.rx_dropped += 1;
                }
                return;
            }
        }
        self.stats.rx_dropped += 1;
    }

    fn input_tcp(&mut self, src: Ipv4Addr, dst: Ipv4Addr, data: &[u8], now: u64) {
        let Some(segment) = wire::parse_tcp(src, dst, data) else {
            self.stats.rx_dropped += 1;
            return;
        };
        let incoming = Incoming {
            seq: segment.seq,
            ack: segment.ack,
            flags: segment.flags,
            window: segment.window,
            mss: segment.mss,
            payload: segment.payload,
        };
        let remote = Endpoint { addr: src, port: segment.src_port };

        let found = self.slots.iter().position(|slot| {
            matches!(&slot.socket, Some(Socket { kind: Kind::Tcp(tcp), .. })
                if tcp.local_port == segment.dst_port && tcp.remote == remote)
        });
        if let Some(index) = found {
            if let Some(Socket { kind: Kind::Tcp(tcp), .. }) = self.slots[index].socket.as_mut() {
                tcp.tcb.on_segment(&incoming, now);
                self.changed = true;
            }
            self.flush_tcp(index, now);
            return;
        }

        let is_syn = segment.flags & (TCP_SYN | TCP_ACK | TCP_RST) == TCP_SYN;
        if is_syn && self.accept_syn(segment.dst_port, remote, &incoming, now) {
            return;
        }
        if let Some(reset) = tcp::reset_for(&incoming) {
            let mut frame = [0u8; MAX_FRAME];
            if let Some(mac) = self.neighbour(remote.addr, now) {
                self.send_segment(&mut frame, mac, segment.dst_port, remote, &reset);
            }
        }
    }

    /// A SYN for a port. Returns `true` if something's listening, even if it's
    /// too busy to take this one: a client tries again, and a reset would stop it.
    fn accept_syn(&mut self, port: u16, remote: Endpoint, segment: &Incoming<'_>, now: u64) -> bool {
        let found = self.slots.iter().enumerate().find_map(|(index, slot)| match &slot.socket {
            Some(Socket { kind: Kind::Listener(listener), owner }) if listener.port == port => {
                Some((make_id(index, slot.generation), listener.backlog, listener.ready.len(), *owner))
            }
            _ => None,
        });
        let Some((id, backlog, ready, owner)) = found else {
            return false;
        };
        let half_open = self
            .slots
            .iter()
            .filter(|slot| matches!(&slot.socket, Some(Socket { kind: Kind::Tcp(tcp), .. }) if tcp.parent == Some(id)))
            .count();
        if ready + half_open >= backlog {
            return true;
        }
        let iss = self.next_random() as u32;
        let tcb = Box::new(Tcb::accept(segment, iss));
        let Ok(index) = self.allocate(Socket {
            owner,
            kind: Kind::Tcp(TcpSocket {
                local_port: port,
                remote,
                tcb,
                parent: Some(id),
                closing: false,
                stranded_since: None,
            }),
        }) else {
            return true;
        };
        self.flush_tcp(index, now);
        true
    }

    // ---- Sockets. ----

    fn allocate(&mut self, socket: Socket) -> Result<usize, NetError> {
        let free = self.slots.iter().position(|slot| slot.socket.is_none());
        let index = match free {
            Some(index) => index,
            None if self.slots.len() < MAX_SOCKETS => {
                self.slots.push(Slot::default());
                self.slots.len() - 1
            }
            None => {
                // TIME-WAIT is the one thing that can go early without harm that
                // matters on this network.
                let stale = self.slots.iter().position(|slot| {
                    matches!(&slot.socket, Some(Socket { kind: Kind::Tcp(tcp), .. })
                        if tcp.closing && tcp.tcb.state() == tcp::State::TimeWait)
                });
                let index = stale.ok_or(NetError::NoResources)?;
                self.slots[index].generation = self.slots[index].generation.wrapping_add(1);
                index
            }
        };
        self.slots[index].socket = Some(socket);
        Ok(index)
    }

    fn resolve_id(&self, id: SocketId) -> Option<usize> {
        let index = (id.0 & 0xffff) as usize;
        let slot = self.slots.get(index)?;
        let live = slot.generation == (id.0 >> 16) as u16 && slot.socket.is_some();
        live.then_some(index)
    }

    fn socket_mut(&mut self, id: SocketId) -> Option<&mut Socket> {
        let index = self.resolve_id(id)?;
        self.slots.get_mut(index)?.socket.as_mut()
    }

    fn tcp_mut(&mut self, id: SocketId) -> Result<(usize, &mut TcpSocket), NetError> {
        let index = self.resolve_id(id).ok_or(NetError::BadSocket)?;
        match self.slots.get_mut(index).and_then(|slot| slot.socket.as_mut()) {
            Some(Socket { kind: Kind::Tcp(tcp), .. }) if !tcp.closing => Ok((index, tcp)),
            _ => Err(NetError::BadSocket),
        }
    }

    fn port_taken_tcp(&self, port: u16) -> bool {
        self.slots.iter().any(|slot| match &slot.socket {
            Some(Socket { kind: Kind::Tcp(tcp), .. }) => tcp.local_port == port,
            Some(Socket { kind: Kind::Listener(listener), .. }) => listener.port == port,
            _ => false,
        })
    }

    fn port_taken_udp(&self, port: u16) -> bool {
        port == dhcp::CLIENT_PORT
            || self.slots.iter().any(|slot| matches!(&slot.socket, Some(Socket { kind: Kind::Udp(udp), .. }) if udp.port == port))
    }

    fn pick_port(&mut self, taken: fn(&Self, u16) -> bool) -> Result<u16, NetError> {
        let range = u32::from(u16::MAX - EPHEMERAL_FIRST) + 1;
        let start = (self.next_random() as u32) % range;
        (0..range)
            .map(|offset| EPHEMERAL_FIRST + ((start + offset) % range) as u16)
            .find(|&port| !taken(self, port))
            .ok_or(NetError::NoResources)
    }

    /// Starts a connection. See whether it's up with [`tcp_connected`](Self::tcp_connected).
    pub fn tcp_connect(&mut self, remote: Endpoint, owner: u32, now: u64) -> Result<SocketId, NetError> {
        if self.addr.is_unspecified() {
            return Err(NetError::NotConfigured);
        }
        if remote.port == 0 || remote.addr.is_unspecified() || remote.addr.is_multicast() || is_broadcast(remote.addr, self.addr, self.netmask) {
            return Err(NetError::Invalid);
        }
        let local_port = self.pick_port(Self::port_taken_tcp)?;
        let iss = self.next_random() as u32;
        let index = self.allocate(Socket {
            owner,
            kind: Kind::Tcp(TcpSocket {
                local_port,
                remote,
                tcb: Box::new(Tcb::connect(iss)),
                parent: None,
                closing: false,
                stranded_since: None,
            }),
        })?;
        let id = make_id(index, self.slots[index].generation);
        self.flush_tcp(index, now);
        Ok(id)
    }

    /// Whether the connection is up. `Err` when it failed.
    pub fn tcp_connected(&mut self, id: SocketId) -> Result<bool, NetError> {
        let (_, tcp) = self.tcp_mut(id)?;
        if let Some(failure) = tcp.tcb.failure() {
            return Err(failure_error(failure));
        }
        if tcp.tcb.is_closed() {
            return Err(NetError::Closed);
        }
        Ok(tcp.tcb.is_connected())
    }

    /// Starts listening.
    pub fn tcp_listen(&mut self, port: u16, backlog: usize, owner: u32) -> Result<SocketId, NetError> {
        if port == 0 {
            return Err(NetError::Invalid);
        }
        let taken = self.slots.iter().any(|slot| matches!(&slot.socket, Some(Socket { kind: Kind::Listener(listener), .. }) if listener.port == port));
        if taken {
            return Err(NetError::AddressInUse);
        }
        let index = self.allocate(Socket {
            owner,
            kind: Kind::Listener(Listener {
                port,
                backlog: backlog.clamp(1, MAX_SOCKETS / 2),
                ready: VecDeque::new(),
            }),
        })?;
        Ok(make_id(index, self.slots[index].generation))
    }

    /// Takes the next established connection, if there is one.
    pub fn tcp_accept(&mut self, id: SocketId, owner: u32) -> Result<Option<(SocketId, Endpoint)>, NetError> {
        let listener = self.listener_mut(id).ok_or(NetError::BadSocket)?;
        let Some(child) = listener.ready.pop_front() else {
            return Ok(None);
        };
        let socket = self.socket_mut(child).ok_or(NetError::BadSocket)?;
        socket.owner = owner;
        let Kind::Tcp(tcp) = &socket.kind else {
            return Err(NetError::BadSocket);
        };
        Ok(Some((child, tcp.remote)))
    }

    /// Queues what fits. `Ok(None)` when there's no room for any of it yet.
    pub fn tcp_send(&mut self, id: SocketId, data: &[u8], now: u64) -> Result<Option<usize>, NetError> {
        let (index, tcp) = self.tcp_mut(id)?;
        if let Some(failure) = tcp.tcb.failure() {
            return Err(failure_error(failure));
        }
        if !tcp.tcb.can_send() {
            return Err(NetError::Closed);
        }
        if data.is_empty() {
            return Ok(Some(0));
        }
        let count = tcp.tcb.send(data);
        if count == 0 {
            return Ok(None);
        }
        self.flush_tcp(index, now);
        Ok(Some(count))
    }

    /// Reads what's there. `Ok(Some(0))` is the end, and `Ok(None)` is nothing yet.
    pub fn tcp_recv(&mut self, id: SocketId, out: &mut [u8], now: u64) -> Result<Option<usize>, NetError> {
        let (index, tcp) = self.tcp_mut(id)?;
        if out.is_empty() {
            return Ok(Some(0));
        }
        let count = tcp.tcb.recv(out);
        if count != 0 {
            self.flush_tcp(index, now);
            return Ok(Some(count));
        }
        if let Some(failure) = tcp.tcb.failure() {
            return Err(failure_error(failure));
        }
        Ok(tcp.tcb.at_end().then_some(0))
    }

    /// Finishes this end's sending.
    pub fn tcp_shutdown(&mut self, id: SocketId, now: u64) -> Result<(), NetError> {
        let (index, tcp) = self.tcp_mut(id)?;
        tcp.tcb.shutdown();
        self.flush_tcp(index, now);
        Ok(())
    }

    /// Whether everything sent has been acknowledged.
    pub fn tcp_flushed(&mut self, id: SocketId) -> Result<bool, NetError> {
        let (_, tcp) = self.tcp_mut(id)?;
        if let Some(failure) = tcp.tcb.failure() {
            return Err(failure_error(failure));
        }
        Ok(tcp.tcb.is_flushed())
    }

    /// Opens a UDP socket. Zero asks for any free port.
    pub fn udp_bind(&mut self, port: u16, owner: u32) -> Result<(SocketId, u16), NetError> {
        let port = if port == 0 {
            self.pick_port(Self::port_taken_udp)?
        } else if self.port_taken_udp(port) {
            return Err(NetError::AddressInUse);
        } else {
            port
        };
        let index = self.allocate(Socket {
            owner,
            kind: Kind::Udp(UdpSocket {
                port,
                queue: VecDeque::new(),
                queued_bytes: 0,
            }),
        })?;
        Ok((make_id(index, self.slots[index].generation), port))
    }

    /// Sends one datagram.
    pub fn udp_send_to(&mut self, id: SocketId, to: Endpoint, data: &[u8], now: u64) -> Result<(), NetError> {
        let port = match self.socket_mut(id).map(|socket| &socket.kind) {
            Some(Kind::Udp(udp)) => udp.port,
            _ => return Err(NetError::BadSocket),
        };
        if self.addr.is_unspecified() {
            return Err(NetError::NotConfigured);
        }
        if to.port == 0 || to.addr.is_unspecified() || data.len() > MAX_DATAGRAM {
            return Err(NetError::Invalid);
        }
        if self.send_udp_from(self.addr, port, to, data, now) {
            Ok(())
        } else {
            Err(NetError::NoResources)
        }
    }

    fn send_udp_from(&mut self, src: Ipv4Addr, port: u16, to: Endpoint, data: &[u8], now: u64) -> bool {
        let mut frame = [0u8; MAX_FRAME];
        if data.len() > MAX_DATAGRAM {
            return false;
        }
        let length = wire::write_udp(&mut frame[ETH_HEADER + IP_HEADER..], (src, port), (to.addr, to.port), data);
        self.send_ip_from(&mut frame, src, to.addr, PROTO_UDP, length, now)
    }

    /// Takes a datagram, if there is one.
    pub fn udp_recv_from(&mut self, id: SocketId, out: &mut [u8]) -> Result<Option<(usize, Endpoint)>, NetError> {
        let Some(Kind::Udp(udp)) = self.socket_mut(id).map(|socket| &mut socket.kind) else {
            return Err(NetError::BadSocket);
        };
        let Some((from, data)) = udp.queue.pop_front() else {
            return Ok(None);
        };
        udp.queued_bytes -= data.len();
        let count = data.len().min(out.len());
        out[..count].copy_from_slice(&data[..count]);
        Ok(Some((count, from)))
    }

    /// Sends an echo request. Look for its answer with [`ping_answered`](Self::ping_answered).
    pub fn ping_start(&mut self, target: Ipv4Addr, sequence: u16, payload: usize, now: u64) -> Result<u16, NetError> {
        if self.addr.is_unspecified() {
            return Err(NetError::NotConfigured);
        }
        if target.is_unspecified() || payload > wire::MTU - IP_HEADER - wire::ICMP_HEADER {
            return Err(NetError::Invalid);
        }
        let id = self.next_random() as u16;
        let mut data = [0u8; wire::MTU];
        for (index, byte) in data[..payload].iter_mut().enumerate() {
            *byte = index as u8;
        }
        let mut frame = [0u8; MAX_FRAME];
        let length = wire::write_icmp_echo(&mut frame[ETH_HEADER + IP_HEADER..], wire::ICMP_ECHO_REQUEST, id, sequence, &data[..payload]);
        if self.pings.len() >= MAX_PINGS {
            self.pings.remove(0);
        }
        self.pings.push(Ping { target, id, sequence, answered: false });
        if !self.send_ip(&mut frame, target, PROTO_ICMP, length, now) {
            self.ping_finish(id);
            return Err(NetError::NoResources);
        }
        Ok(id)
    }

    /// Whether this echo request has been answered.
    pub fn ping_answered(&self, id: u16) -> bool {
        self.pings.iter().any(|ping| ping.id == id && ping.answered)
    }

    /// Forgets an echo request.
    pub fn ping_finish(&mut self, id: u16) {
        self.pings.retain(|ping| ping.id != id);
    }

    /// Closes any socket. A connection stays around, without its owner, until it's
    /// finished what's queued and said goodbye.
    pub fn close(&mut self, id: SocketId, now: u64) {
        let Some(index) = self.resolve_id(id) else {
            return;
        };
        let slot = &mut self.slots[index];
        let Some(socket) = slot.socket.as_mut() else {
            return;
        };
        match &mut socket.kind {
            Kind::Tcp(tcp) => {
                if tcp.closing {
                    return;
                }
                tcp.closing = true;
                tcp.parent = None;
                tcp.tcb.orphan();
                self.flush_tcp(index, now);
            }
            Kind::Listener(listener) => {
                let ready = core::mem::take(&mut listener.ready);
                slot.socket = None;
                slot.generation = slot.generation.wrapping_add(1);
                for child in ready {
                    self.abort(child, now);
                }
            }
            Kind::Udp(_) => {
                slot.socket = None;
                slot.generation = slot.generation.wrapping_add(1);
            }
        }
    }

    fn abort(&mut self, id: SocketId, now: u64) {
        if let Ok((index, tcp)) = self.tcp_mut(id) {
            tcp.tcb.abort();
            tcp.closing = true;
            self.flush_tcp(index, now);
        }
    }

    /// Closes the sockets of whoever's gone: a killed task can't do it itself.
    pub fn close_orphans(&mut self, alive: impl Fn(u32) -> bool, now: u64) {
        for index in 0..self.slots.len() {
            let slot = &self.slots[index];
            let Some(socket) = slot.socket.as_ref() else {
                continue;
            };
            let settled = match &socket.kind {
                Kind::Tcp(tcp) => tcp.closing,
                _ => false,
            };
            if !settled && !alive(socket.owner) {
                self.close(make_id(index, slot.generation), now);
            }
        }
    }

    /// True once the interface has an address.
    pub fn is_configured(&self) -> bool {
        !self.addr.is_unspecified()
    }

    /// The name server, if there is one.
    pub fn name_server(&self) -> Option<Ipv4Addr> {
        (!self.dns.is_unspecified()).then_some(self.dns)
    }
}

/// Most echo data the stack will reflect.
const ICMP_LIMIT: usize = wire::MTU - IP_HEADER - wire::ICMP_HEADER;

const fn make_id(index: usize, generation: u16) -> SocketId {
    SocketId((generation as u32) << 16 | index as u32)
}

const fn failure_error(failure: tcp::Failure) -> NetError {
    match failure {
        tcp::Failure::Refused => NetError::Refused,
        tcp::Failure::Reset => NetError::Reset,
        tcp::Failure::TimedOut => NetError::TimedOut,
    }
}

#[cfg(test)]
mod tests;
