//! Networking, from two sides: what a driver gives the stack, and what the stack
//! gives apps.
//!
//! [`NetDevice`] is one Ethernet interface. [`Network`] is the socket interface apps
//! reach through `feos_app::Platform`. Both are here so that drivers and apps stay
//! apart from `feos-net`, which sits between them.

use core::fmt;
use core::sync::atomic::AtomicU32;

/// An IPv4 address.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash, PartialOrd, Ord)]
pub struct Ipv4Addr(pub [u8; 4]);

impl Ipv4Addr {
    /// `0.0.0.0`.
    pub const UNSPECIFIED: Self = Self([0; 4]);
    /// `255.255.255.255`.
    pub const BROADCAST: Self = Self([255; 4]);

    /// Parses dotted-quad text. Anything else is `None`.
    pub fn parse(text: &str) -> Option<Self> {
        let mut out = [0u8; 4];
        let mut parts = text.split('.');
        for slot in &mut out {
            let part = parts.next()?;
            if part.is_empty() || part.len() > 3 || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            *slot = part.parse().ok()?;
        }
        if parts.next().is_some() {
            return None;
        }
        Some(Self(out))
    }

    /// The address as a host-order number.
    pub const fn to_u32(self) -> u32 {
        u32::from_be_bytes(self.0)
    }

    /// True for `0.0.0.0`.
    pub fn is_unspecified(self) -> bool {
        self == Self::UNSPECIFIED
    }

    /// True for a class D address.
    pub const fn is_multicast(self) -> bool {
        self.0[0] & 0xf0 == 0xe0
    }
}

impl fmt::Display for Ipv4Addr {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d] = self.0;
        write!(out, "{a}.{b}.{c}.{d}")
    }
}

/// One Ethernet interface.
///
/// Frames carry their Ethernet header and no frame check sequence. Calls don't
/// overlap: the stack owns the device.
pub trait NetDevice: Send {
    /// The interface's hardware address.
    fn mac(&self) -> [u8; 6];

    /// Whether the link reports itself up. A device that can't tell says yes.
    fn link_up(&self) -> bool {
        true
    }

    /// Whether [`transmit`](Self::transmit) would take a frame now.
    fn can_transmit(&mut self) -> bool;

    /// Queues one frame. Returns `false` if it wasn't taken: too big, or no room.
    fn transmit(&mut self, frame: &[u8]) -> bool;

    /// Takes the next received frame into `out` and returns its length. A frame
    /// longer than `out` is dropped.
    fn receive(&mut self, out: &mut [u8]) -> Option<usize>;
}

/// A network card a board found, and how to hear from it.
pub struct NetBinding {
    /// The device. It lasts as long as the machine.
    pub device: &'static mut dyn NetDevice,
    /// A word its interrupt handler moves and wakes waiters on.
    pub events: &'static AtomicU32,
    /// How it interrupts, for the log: `"polled"` means it doesn't.
    pub how: &'static str,
}

/// What a socket call can fail with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NetError {
    /// No lease yet, so no address to send from.
    NotConfigured,
    /// The deadline passed before the operation could finish.
    TimedOut,
    /// The calling task has been asked to stop.
    Interrupted,
    /// The peer refused the connection.
    Refused,
    /// The peer reset the connection.
    Reset,
    /// The connection is closed for this direction.
    Closed,
    /// The port is taken.
    AddressInUse,
    /// Out of sockets, or of room for a packet.
    NoResources,
    /// There's no such socket, or it's the wrong kind for this call.
    BadSocket,
    /// The arguments can't be used: a zero port, say, or an oversized datagram.
    Invalid,
    /// The name doesn't exist, or nobody could say.
    NotFound,
}

impl NetError {
    /// A short description, for messages.
    pub const fn message(self) -> &'static str {
        match self {
            Self::NotConfigured => "the network is not configured",
            Self::TimedOut => "timed out",
            Self::Interrupted => "interrupted",
            Self::Refused => "connection refused",
            Self::Reset => "connection reset by peer",
            Self::Closed => "connection closed",
            Self::AddressInUse => "address in use",
            Self::NoResources => "out of network resources",
            Self::BadSocket => "bad socket",
            Self::Invalid => "invalid argument",
            Self::NotFound => "name not found",
        }
    }
}

/// A socket, as apps name it. A closed socket's handle stays invalid.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SocketId(pub u32);

/// A network endpoint.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Endpoint {
    /// Address.
    pub addr: Ipv4Addr,
    /// Port.
    pub port: u16,
}

/// How the interface is set up.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct NetConfig {
    /// Hardware address.
    pub mac: [u8; 6],
    /// Whether the link is up.
    pub link_up: bool,
    /// The interface's address, unspecified until it has a lease.
    pub addr: Ipv4Addr,
    /// Subnet mask.
    pub netmask: Ipv4Addr,
    /// Default gateway.
    pub gateway: Ipv4Addr,
    /// Name server.
    pub dns: Ipv4Addr,
    /// Who gave the lease.
    pub dhcp_server: Ipv4Addr,
    /// Milliseconds left on the lease.
    pub lease_remaining_ms: u64,
}

/// Counters since boot.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct NetStats {
    /// Frames received.
    pub rx_frames: u64,
    /// Bytes in those frames.
    pub rx_bytes: u64,
    /// Frames sent.
    pub tx_frames: u64,
    /// Bytes in those frames.
    pub tx_bytes: u64,
    /// Frames that failed to parse, or weren't for this host.
    pub rx_dropped: u64,
    /// Frames the device wouldn't take.
    pub tx_dropped: u64,
    /// TCP segments sent again.
    pub tcp_retransmits: u64,
    /// TCP connections in use.
    pub tcp_sockets: u32,
    /// UDP sockets in use.
    pub udp_sockets: u32,
}

/// One TCP or UDP socket, for listing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SocketInfo {
    /// Name of the TCP state, or `udp`.
    pub state: &'static str,
    /// Local end.
    pub local: Endpoint,
    /// Remote end, unspecified when it has none.
    pub remote: Endpoint,
    /// Process that opened it.
    pub owner: u32,
}

/// The TCP/IP stack as apps see it.
///
/// Calls that wait take a timeout in milliseconds, and end early with
/// [`NetError::Interrupted`] when the calling task has been asked to stop. A task's
/// sockets are closed for it when it ends without doing so.
pub trait Network: Sync {
    /// How the interface is set up now.
    fn config(&self) -> NetConfig;
    /// Counters since boot.
    fn stats(&self) -> NetStats;
    /// Copies a listing of open sockets into `out` and returns its length.
    fn sockets(&self, out: &mut [SocketInfo]) -> usize;
    /// Waits for an address. Returns whether there is one.
    fn wait_configured(&self, timeout_ms: u64) -> bool;

    /// Runs the stack: takes in what arrived, sends what's due, and sleeps for up to
    /// `max_wait_ms` or until something happens. The network task's whole job.
    fn service(&self, max_wait_ms: u64);

    /// Sends one echo request and waits for its reply. The answer is the round trip
    /// in microseconds, as near as the clock tells.
    fn ping(&self, target: Ipv4Addr, sequence: u16, payload: usize, timeout_ms: u64) -> Result<u64, NetError>;
    /// Finds a name's address through the configured name server.
    fn resolve(&self, name: &str, timeout_ms: u64) -> Result<Ipv4Addr, NetError>;

    /// Opens a TCP connection, and waits for it to be established.
    fn tcp_connect(&self, remote: Endpoint, timeout_ms: u64) -> Result<SocketId, NetError>;
    /// Starts listening on a port.
    fn tcp_listen(&self, port: u16, backlog: usize) -> Result<SocketId, NetError>;
    /// Waits for a connection to a listener, and returns it and who's on the far end.
    fn tcp_accept(&self, listener: SocketId, timeout_ms: u64) -> Result<(SocketId, Endpoint), NetError>;
    /// Queues as much of `data` as there's room for, after waiting for some room.
    /// Returns how much it took.
    fn tcp_send(&self, socket: SocketId, data: &[u8], timeout_ms: u64) -> Result<usize, NetError>;
    /// Reads what's there, after waiting for something. Zero means the peer has
    /// finished sending and everything's been read.
    fn tcp_recv(&self, socket: SocketId, out: &mut [u8], timeout_ms: u64) -> Result<usize, NetError>;
    /// Says this end has finished sending. Reading goes on.
    fn tcp_shutdown(&self, socket: SocketId) -> Result<(), NetError>;
    /// Waits until everything queued has been acknowledged.
    fn tcp_flush(&self, socket: SocketId, timeout_ms: u64) -> Result<(), NetError>;

    /// Opens a UDP socket on a port, or on a free one for zero, and returns it and
    /// the port.
    fn udp_bind(&self, port: u16) -> Result<(SocketId, u16), NetError>;
    /// Sends one datagram.
    fn udp_send_to(&self, socket: SocketId, to: Endpoint, data: &[u8]) -> Result<(), NetError>;
    /// Waits for a datagram. Whatever doesn't fit in `out` is lost.
    fn udp_recv_from(&self, socket: SocketId, out: &mut [u8], timeout_ms: u64) -> Result<(usize, Endpoint), NetError>;

    /// Closes any socket. A TCP connection finishes sending what's queued first.
    fn close(&self, socket: SocketId);
}
