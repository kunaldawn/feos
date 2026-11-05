//! A DHCP client (RFC 2131): discover, request, renew, rebind.
//!
//! [`Client::poll`] says what to send and when, and [`Client::on_message`] takes
//! what comes back. The lease it holds is what configures the interface.

use alloc::vec::Vec;

use feos_hal::net::Ipv4Addr;

/// The port servers listen on.
pub const SERVER_PORT: u16 = 67;
/// The port clients listen on.
pub const CLIENT_PORT: u16 = 68;

const COOKIE: [u8; 4] = [99, 130, 83, 99];
/// Bytes before the options: the fixed fields and the cookie.
const OPTIONS_AT: usize = 240;

const DISCOVER: u8 = 1;
const OFFER: u8 = 2;
const REQUEST: u8 = 3;
const ACK: u8 = 5;
const NAK: u8 = 6;

/// First retransmission, in milliseconds. It doubles each time.
const FIRST_RETRY_MS: u64 = 2000;
/// Longest gap between retransmissions.
const MAX_RETRY_MS: u64 = 32_000;
/// Attempts at a REQUEST before starting over.
const MAX_REQUESTS: u32 = 4;
/// A lease too short to believe.
const MIN_LEASE_S: u32 = 10;

/// The options of a server's message, as far as this client cares.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Message {
    /// Transaction ID.
    pub xid: u32,
    /// Kind of message.
    pub kind: u8,
    /// The address on offer.
    pub your_addr: Ipv4Addr,
    /// Server identifier.
    pub server: Ipv4Addr,
    /// Subnet mask.
    pub netmask: Ipv4Addr,
    /// First router.
    pub router: Ipv4Addr,
    /// First name server.
    pub dns: Ipv4Addr,
    /// Lease time, in seconds.
    pub lease_s: u32,
    /// Renewal time, in seconds.
    pub renew_s: u32,
    /// Rebinding time, in seconds.
    pub rebind_s: u32,
}

fn addr_at(data: &[u8], at: usize) -> Option<Ipv4Addr> {
    let bytes = data.get(at..at.checked_add(4)?)?;
    Some(Ipv4Addr([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    let bytes = data.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Reads a server's message to a client with this hardware address. `None` for
/// anything else.
pub fn parse(data: &[u8], mac: [u8; 6]) -> Option<Message> {
    // A reply, on Ethernet, with six-byte hardware addresses.
    if *data.first()? != 2 || *data.get(1)? != 1 || *data.get(2)? != 6 {
        return None;
    }
    if data.get(28..34)? != mac || data.get(236..240)? != COOKIE {
        return None;
    }
    let mut message = Message {
        xid: u32_at(data, 4)?,
        your_addr: addr_at(data, 16)?,
        ..Message::default()
    };
    let mut at = OPTIONS_AT;
    while let Some(&code) = data.get(at) {
        match code {
            0 => at += 1,
            255 => break,
            _ => {
                let length = usize::from(*data.get(at + 1)?);
                let value = data.get(at + 2..at + 2 + length)?;
                match (code, length) {
                    (53, 1) => message.kind = value[0],
                    (54, 4) => message.server = addr_at(value, 0)?,
                    (1, 4) => message.netmask = addr_at(value, 0)?,
                    (3, 4..) => message.router = addr_at(value, 0)?,
                    (6, 4..) => message.dns = addr_at(value, 0)?,
                    (51, 4) => message.lease_s = u32_at(value, 0)?,
                    (58, 4) => message.renew_s = u32_at(value, 0)?,
                    (59, 4) => message.rebind_s = u32_at(value, 0)?,
                    _ => {}
                }
                at += 2 + length;
            }
        }
    }
    (message.kind != 0).then_some(message)
}

/// Writes a client's message.
fn build(kind: u8, xid: u32, mac: [u8; 6], ciaddr: Ipv4Addr, requested: Option<Ipv4Addr>, server: Option<Ipv4Addr>, broadcast: bool) -> Vec<u8> {
    let mut out = alloc::vec![0u8; OPTIONS_AT];
    out[0] = 1;
    out[1] = 1;
    out[2] = 6;
    out[4..8].copy_from_slice(&xid.to_be_bytes());
    if broadcast {
        out[10] = 0x80;
    }
    out[12..16].copy_from_slice(&ciaddr.0);
    out[28..34].copy_from_slice(&mac);
    out[236..240].copy_from_slice(&COOKIE);
    out.extend_from_slice(&[53, 1, kind]);
    if let Some(requested) = requested {
        out.extend_from_slice(&[50, 4]);
        out.extend_from_slice(&requested.0);
    }
    if let Some(server) = server {
        out.extend_from_slice(&[54, 4]);
        out.extend_from_slice(&server.0);
    }
    // Asks for: mask, router, DNS, lease, renewal and rebinding times.
    out.extend_from_slice(&[55, 6, 1, 3, 6, 51, 58, 59]);
    // The client's name, which a real network shows in its lease table.
    out.extend_from_slice(&[12, 4]);
    out.extend_from_slice(b"feos");
    out.push(255);
    out
}

/// A lease that's in force.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Lease {
    /// This host's address.
    pub addr: Ipv4Addr,
    /// Subnet mask.
    pub netmask: Ipv4Addr,
    /// Default gateway.
    pub router: Ipv4Addr,
    /// Name server.
    pub dns: Ipv4Addr,
    /// Who gave it.
    pub server: Ipv4Addr,
    /// When it was given, or renewed.
    obtained_ms: u64,
    /// When to ask the server to renew.
    renew_ms: u64,
    /// When to ask anyone.
    rebind_ms: u64,
    /// When it ends.
    expires_ms: u64,
}

impl Lease {
    /// Milliseconds left on it.
    pub const fn remaining_ms(&self, now: u64) -> u64 {
        self.expires_ms.saturating_sub(now)
    }

    /// When it was last given.
    pub const fn obtained_ms(&self) -> u64 {
        self.obtained_ms
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// Looking for a server.
    Selecting,
    /// One made an offer, and this is the answer.
    Requesting { offer: Ipv4Addr, server: Ipv4Addr, tries: u32 },
    /// All set until it's time to renew.
    Bound,
    /// Asking the server to extend it.
    Renewing,
    /// Asking anyone to extend it.
    Rebinding,
}

/// What's to be sent.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Send {
    /// The message.
    pub payload: Vec<u8>,
    /// From this address, the unspecified one before there's a lease.
    pub from: Ipv4Addr,
    /// To this address.
    pub to: Ipv4Addr,
}

/// What a message did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Change {
    /// Nothing visible.
    None,
    /// There's a new or extended lease.
    Leased,
    /// The address is gone.
    Lost,
}

/// One interface's DHCP client.
pub struct Client {
    mac: [u8; 6],
    xid: u32,
    state: State,
    lease: Option<Lease>,
    next_send_ms: u64,
    retry_ms: u64,
}

impl Client {
    /// A client that starts looking at once. `xid` should vary from boot to boot.
    pub const fn new(mac: [u8; 6], xid: u32) -> Self {
        Self {
            mac,
            xid,
            state: State::Selecting,
            lease: None,
            next_send_ms: 0,
            retry_ms: FIRST_RETRY_MS,
        }
    }

    /// The lease in force.
    pub const fn lease(&self) -> Option<&Lease> {
        self.lease.as_ref()
    }

    /// When [`poll`](Self::poll) next has something to do.
    pub fn next_deadline(&self) -> u64 {
        match (self.state, self.lease) {
            (State::Bound, Some(lease)) => lease.renew_ms,
            (State::Renewing | State::Rebinding, Some(lease)) => self.next_send_ms.min(lease.expires_ms).min(
                if self.state == State::Renewing { lease.rebind_ms } else { u64::MAX },
            ),
            _ => self.next_send_ms,
        }
    }

    fn restart(&mut self, now: u64) -> Change {
        let had = self.lease.take().is_some();
        self.state = State::Selecting;
        self.xid = self.xid.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.next_send_ms = now;
        self.retry_ms = FIRST_RETRY_MS;
        if had {
            Change::Lost
        } else {
            Change::None
        }
    }

    /// Lets time pass, and returns what to send and any change that made.
    pub fn poll(&mut self, now: u64) -> (Option<Send>, Change) {
        // Timers that move the state along.
        let mut change = Change::None;
        if let Some(lease) = self.lease {
            if now >= lease.expires_ms {
                change = self.restart(now);
            } else if now >= lease.rebind_ms && matches!(self.state, State::Bound | State::Renewing) {
                self.state = State::Rebinding;
                self.next_send_ms = now;
                self.retry_ms = FIRST_RETRY_MS;
            } else if now >= lease.renew_ms && self.state == State::Bound {
                self.state = State::Renewing;
                self.next_send_ms = now;
                self.retry_ms = FIRST_RETRY_MS;
            }
        }
        if self.state == State::Bound || now < self.next_send_ms {
            return (None, change);
        }
        let send = match self.state {
            State::Selecting => Send {
                payload: build(DISCOVER, self.xid, self.mac, Ipv4Addr::UNSPECIFIED, None, None, true),
                from: Ipv4Addr::UNSPECIFIED,
                to: Ipv4Addr::BROADCAST,
            },
            State::Requesting { offer, server, tries } => {
                if tries >= MAX_REQUESTS {
                    return (None, self.restart(now));
                }
                self.state = State::Requesting { offer, server, tries: tries + 1 };
                Send {
                    payload: build(REQUEST, self.xid, self.mac, Ipv4Addr::UNSPECIFIED, Some(offer), Some(server), true),
                    from: Ipv4Addr::UNSPECIFIED,
                    to: Ipv4Addr::BROADCAST,
                }
            }
            State::Renewing | State::Rebinding => {
                let Some(lease) = self.lease else {
                    return (None, self.restart(now));
                };
                let renewing = self.state == State::Renewing;
                Send {
                    payload: build(REQUEST, self.xid, self.mac, lease.addr, None, None, !renewing),
                    from: lease.addr,
                    to: if renewing { lease.server } else { Ipv4Addr::BROADCAST },
                }
            }
            State::Bound => return (None, change),
        };
        self.next_send_ms = now + self.retry_ms;
        self.retry_ms = (self.retry_ms * 2).min(MAX_RETRY_MS);
        (Some(send), change)
    }

    /// Takes a message from a server.
    pub fn on_message(&mut self, data: &[u8], now: u64) -> Change {
        let Some(message) = parse(data, self.mac) else {
            return Change::None;
        };
        if message.xid != self.xid {
            return Change::None;
        }
        match (self.state, message.kind) {
            (State::Selecting, OFFER) => {
                if message.your_addr.is_unspecified() || message.server.is_unspecified() {
                    return Change::None;
                }
                self.state = State::Requesting {
                    offer: message.your_addr,
                    server: message.server,
                    tries: 0,
                };
                self.next_send_ms = now;
                self.retry_ms = FIRST_RETRY_MS;
                Change::None
            }
            (State::Requesting { .. } | State::Renewing | State::Rebinding, ACK) => {
                if message.your_addr.is_unspecified() || message.your_addr.is_multicast() || message.your_addr == Ipv4Addr::BROADCAST {
                    return Change::None;
                }
                self.accept(&message, now);
                Change::Leased
            }
            (State::Requesting { .. } | State::Renewing | State::Rebinding, NAK) => self.restart(now),
            _ => Change::None,
        }
    }

    fn accept(&mut self, message: &Message, now: u64) {
        let lease_s = if message.lease_s == 0 { 3600 } else { message.lease_s.max(MIN_LEASE_S) };
        let seconds = |value: u32, default: u32| -> u64 {
            let value = if value == 0 || value >= lease_s { default } else { value };
            u64::from(value) * 1000
        };
        let server = if message.server.is_unspecified() {
            self.lease.map_or(Ipv4Addr::UNSPECIFIED, |lease| lease.server)
        } else {
            message.server
        };
        self.lease = Some(Lease {
            addr: message.your_addr,
            netmask: message.netmask,
            router: message.router,
            dns: message.dns,
            server,
            obtained_ms: now,
            renew_ms: now + seconds(message.renew_s, lease_s / 2),
            rebind_ms: now + seconds(message.rebind_s, lease_s / 8 * 7),
            expires_ms: now + u64::from(lease_s) * 1000,
        });
        self.state = State::Bound;
        self.retry_ms = FIRST_RETRY_MS;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: [u8; 6] = [2, 0, 0, 0, 0, 1];
    const OFFERED: Ipv4Addr = Ipv4Addr([10, 0, 2, 15]);
    const SERVER: Ipv4Addr = Ipv4Addr([10, 0, 2, 2]);

    fn reply(xid: u32, kind: u8, extra: &[u8]) -> Vec<u8> {
        let mut out = alloc::vec![0u8; OPTIONS_AT];
        out[0] = 2;
        out[1] = 1;
        out[2] = 6;
        out[4..8].copy_from_slice(&xid.to_be_bytes());
        out[16..20].copy_from_slice(&OFFERED.0);
        out[28..34].copy_from_slice(&MAC);
        out[236..240].copy_from_slice(&COOKIE);
        out.extend_from_slice(&[53, 1, kind, 54, 4]);
        out.extend_from_slice(&SERVER.0);
        out.extend_from_slice(extra);
        out.push(255);
        out
    }

    const LEASE: &[u8] = &[
        1, 4, 255, 255, 255, 0, 3, 4, 10, 0, 2, 2, 6, 8, 10, 0, 2, 3, 8, 8, 8, 8, 51, 4, 0, 0, 0, 100,
    ];

    fn xid_of(send: &Send) -> u32 {
        u32_at(&send.payload, 4).unwrap()
    }

    fn bound_client() -> Client {
        let mut client = Client::new(MAC, 77);
        let (send, _) = client.poll(0);
        let xid = xid_of(&send.unwrap());
        client.on_message(&reply(xid, OFFER, &[]), 1);
        let (send, _) = client.poll(1);
        assert!(send.is_some());
        assert_eq!(client.on_message(&reply(xid, ACK, LEASE), 2), Change::Leased);
        client
    }

    #[test]
    fn goes_from_discover_to_a_lease() {
        let client = bound_client();
        let lease = client.lease().unwrap();
        assert_eq!(lease.addr, OFFERED);
        assert_eq!(lease.netmask, Ipv4Addr([255, 255, 255, 0]));
        assert_eq!(lease.router, SERVER);
        assert_eq!(lease.dns, Ipv4Addr([10, 0, 2, 3]));
        assert_eq!(lease.remaining_ms(2), 100_000);
    }

    #[test]
    fn a_discover_is_laid_out_correctly() {
        let mut client = Client::new(MAC, 77);
        let (send, _) = client.poll(0);
        let send = send.unwrap();
        assert_eq!((send.from, send.to), (Ipv4Addr::UNSPECIFIED, Ipv4Addr::BROADCAST));
        let data = send.payload;
        assert_eq!((data[0], data[1], data[2]), (1, 1, 6));
        assert_eq!(data[10], 0x80, "asks for a broadcast answer");
        assert_eq!(&data[28..34], &MAC);
        assert_eq!(&data[236..240], &COOKIE);
        assert_eq!(&data[240..243], &[53, 1, DISCOVER]);
        assert_eq!(*data.last().unwrap(), 255);
    }

    #[test]
    fn retransmits_with_backoff() {
        let mut client = Client::new(MAC, 77);
        let mut sends = Vec::new();
        for now in (0..200_000).step_by(100) {
            if client.poll(now).0.is_some() {
                sends.push(now);
            }
        }
        assert_eq!(&sends[..5], &[0, 2000, 6000, 14000, 30000]);
        assert_eq!(sends[6] - sends[5], MAX_RETRY_MS);
    }

    #[test]
    fn someone_elses_messages_are_ignored() {
        let mut client = Client::new(MAC, 77);
        let (send, _) = client.poll(0);
        let xid = xid_of(&send.unwrap());
        // Another transaction.
        client.on_message(&reply(xid ^ 1, OFFER, &[]), 1);
        assert_eq!(client.state, State::Selecting);
        // Another machine.
        let mut other = reply(xid, OFFER, &[]);
        other[28] ^= 1;
        client.on_message(&other, 1);
        assert_eq!(client.state, State::Selecting);
        // An ACK out of nowhere.
        assert_eq!(client.on_message(&reply(xid, ACK, LEASE), 1), Change::None);
        assert!(client.lease().is_none());
    }

    #[test]
    fn renews_at_half_time_from_the_server_and_rebinds_later() {
        let mut client = bound_client();
        assert_eq!(client.next_deadline(), 2 + 50_000);
        assert!(client.poll(1000).0.is_none());
        let renew = client.poll(52_002).0.unwrap();
        assert_eq!((renew.from, renew.to), (OFFERED, SERVER));
        let rebind = client.poll(2 + 87_500).0.unwrap();
        assert_eq!((rebind.from, rebind.to), (OFFERED, Ipv4Addr::BROADCAST));
        let xid = xid_of(&rebind);
        assert_eq!(client.on_message(&reply(xid, ACK, LEASE), 90_000), Change::Leased);
        assert_eq!(client.lease().unwrap().remaining_ms(90_000), 100_000);
        assert_eq!(client.state, State::Bound);
    }

    #[test]
    fn the_address_goes_when_the_lease_runs_out() {
        let mut client = bound_client();
        let _ = client.poll(52_002);
        let (send, change) = client.poll(200_000);
        assert_eq!(change, Change::Lost);
        assert!(client.lease().is_none());
        assert_eq!(send.unwrap().from, Ipv4Addr::UNSPECIFIED);
    }

    #[test]
    fn a_refusal_starts_over() {
        let mut client = Client::new(MAC, 77);
        let (send, _) = client.poll(0);
        let xid = xid_of(&send.unwrap());
        client.on_message(&reply(xid, OFFER, &[]), 1);
        client.poll(1);
        client.on_message(&reply(xid, NAK, &[]), 2);
        assert_eq!(client.state, State::Selecting);
    }

    #[test]
    fn an_offer_that_never_gets_an_ack_starts_over() {
        let mut client = Client::new(MAC, 77);
        let (send, _) = client.poll(0);
        let xid = xid_of(&send.unwrap());
        client.on_message(&reply(xid, OFFER, &[]), 1);
        for now in (1..120_000).step_by(1000) {
            client.poll(now);
        }
        assert_eq!(client.state, State::Selecting);
    }

    #[test]
    fn option_lists_that_lie_are_refused() {
        let good = reply(1, OFFER, LEASE);
        assert!(parse(&good, MAC).is_some());
        let mut bad = good.clone();
        let end = bad.len() - 1;
        bad[end] = 51;
        assert!(parse(&bad, MAC).is_none(), "an option with no length");
        let mut bad = good.clone();
        bad.truncate(end);
        bad.extend_from_slice(&[51, 200, 1]);
        assert!(parse(&bad, MAC).is_none(), "an option running off the end");
        assert!(parse(&good[..200], MAC).is_none());
    }

    #[test]
    fn no_input_panics() {
        let mut state = 0x7777_1234_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let good = reply(1, OFFER, LEASE);
        let mut client = Client::new(MAC, 1);
        for round in 0..200_000usize {
            let mut data = good.clone();
            for _ in 0..1 + round % 8 {
                let at = OPTIONS_AT + next() as usize % (data.len() - OPTIONS_AT);
                data[at] = next() as u8;
            }
            let length = data.len() - round % 40;
            let _ = parse(&data[..length], MAC);
            client.on_message(&data[..length], round as u64);
        }
    }
}
