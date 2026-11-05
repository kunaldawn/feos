//! The stack against a fake device: the frames it sends are what a peer would see,
//! and a small peer in the test answers them.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::*;
use crate::dns;
use crate::wire::*;

const MAC: [u8; 6] = [2, 0, 0, 0, 0, 1];
const PEER_MAC: [u8; 6] = [2, 0, 0, 0, 0, 2];
const ADDR: Ipv4Addr = Ipv4Addr([10, 0, 2, 15]);
const GATEWAY: Ipv4Addr = Ipv4Addr([10, 0, 2, 2]);
const DNS: Ipv4Addr = Ipv4Addr([10, 0, 2, 3]);
const OUTSIDE: Ipv4Addr = Ipv4Addr([93, 184, 216, 34]);

#[derive(Default)]
struct Queues {
    rx: VecDeque<Vec<u8>>,
    tx: VecDeque<Vec<u8>>,
    full: bool,
}

#[derive(Clone, Default)]
struct Fake(Arc<Mutex<Queues>>);

impl NetDevice for Fake {
    fn mac(&self) -> [u8; 6] {
        MAC
    }
    fn can_transmit(&mut self) -> bool {
        !self.0.lock().unwrap().full
    }
    fn transmit(&mut self, frame: &[u8]) -> bool {
        let mut queues = self.0.lock().unwrap();
        if queues.full {
            return false;
        }
        queues.tx.push_back(frame.to_vec());
        true
    }
    fn receive(&mut self, out: &mut [u8]) -> Option<usize> {
        let frame = self.0.lock().unwrap().rx.pop_front()?;
        out[..frame.len()].copy_from_slice(&frame);
        Some(frame.len())
    }
}

fn dhcp_reply(request: &[u8], kind: u8) -> Vec<u8> {
    let mut out = vec![0u8; 240];
    out[0] = 2;
    out[1] = 1;
    out[2] = 6;
    out[4..8].copy_from_slice(&request[4..8]);
    out[16..20].copy_from_slice(&ADDR.0);
    out[28..34].copy_from_slice(&request[28..34]);
    out[236..240].copy_from_slice(&[99, 130, 83, 99]);
    out.extend_from_slice(&[53, 1, kind, 54, 4, 10, 0, 2, 2]);
    out.extend_from_slice(&[1, 4, 255, 255, 255, 0, 3, 4, 10, 0, 2, 2, 6, 4, 10, 0, 2, 3]);
    out.extend_from_slice(&[51, 4, 0, 1, 0x51, 0x80, 255]);
    out
}

/// Builds an IP packet in an Ethernet frame to this host.
fn frame_to_host(src: Ipv4Addr, protocol: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0u8; ETH_HEADER + IP_HEADER + payload.len()];
    write_ethernet(&mut frame, MAC, PEER_MAC, ETHERTYPE_IPV4);
    write_ipv4(&mut frame[ETH_HEADER..], src, ADDR, protocol, payload.len(), 1);
    frame[ETH_HEADER + IP_HEADER..].copy_from_slice(payload);
    frame
}

fn udp_to_host(src: Ipv4Addr, src_port: u16, dst_port: u16, data: &[u8]) -> Vec<u8> {
    let mut payload = vec![0u8; UDP_HEADER + data.len()];
    write_udp(&mut payload, (src, src_port), (ADDR, dst_port), data);
    frame_to_host(src, PROTO_UDP, &payload)
}

/// A TCP segment to this host, in an Ethernet frame.
fn tcp_to_host(src: Ipv4Addr, header: &TcpHeader, data: &[u8]) -> Vec<u8> {
    let offset = tcp_data_offset(header.mss.is_some());
    let mut segment = vec![0u8; offset + data.len()];
    segment[offset..].copy_from_slice(data);
    write_tcp(&mut segment, src, ADDR, header, data.len());
    frame_to_host(src, PROTO_TCP, &segment)
}

struct Rig {
    stack: Stack,
    device: Fake,
    now: u64,
}

impl Rig {
    fn new() -> Self {
        let device = Fake::default();
        let stack = Stack::new(Box::new(device.clone()), 42);
        Self { stack, device, now: 1000 }
    }

    /// A stack with a lease, and the gateway's address known.
    fn configured() -> Self {
        let mut rig = Self::new();
        rig.lease();
        rig.answer_arp_for(GATEWAY);
        rig
    }

    fn lease(&mut self) {
        self.poll();
        let discover = self.take_frames().pop().expect("a discover");
        let ip = parse_ipv4(&discover[ETH_HEADER..]).unwrap();
        let udp = parse_udp(ip.src, ip.dst, ip.payload).unwrap();
        assert_eq!((udp.src_port, udp.dst_port), (68, 67));
        let offer = dhcp_reply(udp.payload, 2);
        self.deliver(udp_to_host(GATEWAY, 67, 68, &offer));
        self.poll();
        let request = self.take_frames().pop().expect("a request");
        let ip = parse_ipv4(&request[ETH_HEADER..]).unwrap();
        let udp = parse_udp(ip.src, ip.dst, ip.payload).unwrap();
        let ack = dhcp_reply(udp.payload, 5);
        self.deliver(udp_to_host(GATEWAY, 67, 68, &ack));
        self.poll();
        assert_eq!(self.stack.config(self.now).addr, ADDR);
    }

    /// Answers as `who`, telling it the peer's MAC.
    fn answer_arp_for(&mut self, who: Ipv4Addr) {
        let packet = Arp {
            operation: ARP_REPLY,
            sender_mac: PEER_MAC,
            sender_ip: who,
            target_mac: MAC,
            target_ip: ADDR,
        };
        let mut frame = [0u8; ETH_HEADER + ARP_LEN];
        write_ethernet(&mut frame, MAC, PEER_MAC, ETHERTYPE_ARP);
        write_arp(&mut frame[ETH_HEADER..], &packet);
        self.deliver(frame.to_vec());
        self.poll();
        self.take_frames();
    }

    fn deliver(&mut self, frame: Vec<u8>) {
        self.device.0.lock().unwrap().rx.push_back(frame);
    }

    fn poll(&mut self) -> PollResult {
        self.stack.poll(self.now)
    }

    fn take_frames(&mut self) -> Vec<Vec<u8>> {
        self.device.0.lock().unwrap().tx.drain(..).collect()
    }

    /// The next TCP segment sent, parsed, with its data copied out.
    fn tcp_sent(&mut self) -> Vec<(TcpHeader, Vec<u8>)> {
        self.take_frames()
            .iter()
            .filter_map(|frame| {
                let ethernet = parse_ethernet(frame)?;
                let ip = parse_ipv4(ethernet.payload)?;
                let tcp = parse_tcp(ip.src, ip.dst, ip.payload)?;
                Some((
                    TcpHeader {
                        src_port: tcp.src_port,
                        dst_port: tcp.dst_port,
                        seq: tcp.seq,
                        ack: tcp.ack,
                        flags: tcp.flags,
                        window: tcp.window,
                        mss: tcp.mss,
                    },
                    tcp.payload.to_vec(),
                ))
            })
            .collect()
    }
}

#[test]
fn takes_a_lease_and_answers_arp_for_its_address() {
    let mut rig = Rig::new();
    rig.lease();
    let config = rig.stack.config(rig.now);
    assert_eq!((config.netmask, config.gateway, config.dns), (Ipv4Addr([255, 255, 255, 0]), GATEWAY, DNS));
    assert_eq!(config.lease_remaining_ms, 86_400_000);

    let question = Arp {
        operation: ARP_REQUEST,
        sender_mac: PEER_MAC,
        sender_ip: GATEWAY,
        target_mac: [0; 6],
        target_ip: ADDR,
    };
    let mut frame = [0u8; ETH_HEADER + ARP_LEN];
    write_ethernet(&mut frame, MAC_BROADCAST, PEER_MAC, ETHERTYPE_ARP);
    write_arp(&mut frame[ETH_HEADER..], &question);
    rig.deliver(frame.to_vec());
    rig.poll();
    let reply = rig.take_frames().pop().unwrap();
    let parsed = parse_arp(&reply[ETH_HEADER..]).unwrap();
    assert_eq!((parsed.operation, parsed.sender_ip, parsed.target_ip), (ARP_REPLY, ADDR, GATEWAY));
    assert_eq!(&reply[..6], &PEER_MAC);
}

#[test]
fn answers_a_ping_and_says_when_its_own_is_answered() {
    let mut rig = Rig::configured();
    let mut echo = [0u8; 64];
    let length = write_icmp_echo(&mut echo, ICMP_ECHO_REQUEST, 5, 6, b"payload");
    rig.deliver(frame_to_host(GATEWAY, PROTO_ICMP, &echo[..length]));
    rig.poll();
    let reply = rig.take_frames().pop().unwrap();
    let ip = parse_ipv4(&reply[ETH_HEADER..]).unwrap();
    let parsed = parse_icmp_echo(ip.payload).unwrap();
    assert_eq!((parsed.kind, parsed.id, parsed.sequence, parsed.data), (ICMP_ECHO_REPLY, 5, 6, &b"payload"[..]));

    let id = rig.stack.ping_start(GATEWAY, 1, 8, rig.now).unwrap();
    let request = rig.take_frames().pop().unwrap();
    let ip = parse_ipv4(&request[ETH_HEADER..]).unwrap();
    let sent = parse_icmp_echo(ip.payload).unwrap();
    assert!(!rig.stack.ping_answered(id));
    let length = write_icmp_echo(&mut echo, ICMP_ECHO_REPLY, sent.id, sent.sequence, sent.data);
    rig.deliver(frame_to_host(GATEWAY, PROTO_ICMP, &echo[..length]));
    rig.poll();
    assert!(rig.stack.ping_answered(id));
}

#[test]
fn a_packet_for_an_unknown_neighbour_waits_for_its_arp_answer() {
    let mut rig = Rig::new();
    rig.lease();
    let id = rig.stack.ping_start(GATEWAY, 1, 8, rig.now).unwrap();
    let asked = rig.take_frames().pop().unwrap();
    let packet = parse_arp(&asked[ETH_HEADER..]).unwrap();
    assert_eq!((packet.operation, packet.target_ip), (ARP_REQUEST, GATEWAY));
    rig.answer_arp_for_keeping(GATEWAY);
    let frames = rig.take_frames();
    let ip = parse_ipv4(&frames[0][ETH_HEADER..]).unwrap();
    assert_eq!(ip.protocol, PROTO_ICMP);
    assert_eq!(&frames[0][..6], &PEER_MAC);
    rig.stack.ping_finish(id);
}

impl Rig {
    fn answer_arp_for_keeping(&mut self, who: Ipv4Addr) {
        let packet = Arp {
            operation: ARP_REPLY,
            sender_mac: PEER_MAC,
            sender_ip: who,
            target_mac: MAC,
            target_ip: ADDR,
        };
        let mut frame = [0u8; ETH_HEADER + ARP_LEN];
        write_ethernet(&mut frame, MAC, PEER_MAC, ETHERTYPE_ARP);
        write_arp(&mut frame[ETH_HEADER..], &packet);
        self.deliver(frame.to_vec());
        self.poll();
    }
}

#[test]
fn off_link_traffic_goes_through_the_gateway() {
    let mut rig = Rig::configured();
    rig.stack.ping_start(OUTSIDE, 1, 0, rig.now).unwrap();
    let frame = rig.take_frames().pop().unwrap();
    assert_eq!(&frame[..6], &PEER_MAC);
    assert_eq!(parse_ipv4(&frame[ETH_HEADER..]).unwrap().dst, OUTSIDE);
}

#[test]
fn udp_goes_both_ways_and_a_full_queue_drops() {
    let mut rig = Rig::configured();
    let (socket, port) = rig.stack.udp_bind(5000, 1).unwrap();
    assert_eq!(port, 5000);
    assert_eq!(rig.stack.udp_bind(5000, 1), Err(NetError::AddressInUse));
    rig.stack.udp_send_to(socket, Endpoint { addr: GATEWAY, port: 7 }, b"hello", rig.now).unwrap();
    let frame = rig.take_frames().pop().unwrap();
    let ip = parse_ipv4(&frame[ETH_HEADER..]).unwrap();
    assert_eq!(parse_udp(ip.src, ip.dst, ip.payload).unwrap().payload, b"hello");

    for index in 0..UDP_QUEUE + 5 {
        rig.deliver(udp_to_host(GATEWAY, 7, 5000, &[index as u8]));
        rig.poll();
    }
    let mut out = [0u8; 16];
    assert_eq!(rig.stack.udp_recv_from(socket, &mut out).unwrap(), Some((1, Endpoint { addr: GATEWAY, port: 7 })));
    assert_eq!(out[0], 0);
    let mut count = 1;
    while rig.stack.udp_recv_from(socket, &mut out).unwrap().is_some() {
        count += 1;
    }
    assert_eq!(count, UDP_QUEUE);
}

#[test]
fn oversized_datagrams_and_empty_addresses_are_refused() {
    let mut rig = Rig::configured();
    let (socket, _) = rig.stack.udp_bind(0, 1).unwrap();
    let big = vec![0u8; MAX_DATAGRAM + 1];
    let to = Endpoint { addr: GATEWAY, port: 7 };
    assert_eq!(rig.stack.udp_send_to(socket, to, &big, rig.now), Err(NetError::Invalid));
    assert_eq!(rig.stack.udp_send_to(socket, Endpoint { addr: GATEWAY, port: 0 }, b"", rig.now), Err(NetError::Invalid));
    assert!(rig.stack.udp_send_to(socket, to, &big[..MAX_DATAGRAM], rig.now).is_ok());
}

#[test]
fn nothing_works_without_an_address() {
    let mut rig = Rig::new();
    let remote = Endpoint { addr: GATEWAY, port: 80 };
    assert_eq!(rig.stack.tcp_connect(remote, 1, 0), Err(NetError::NotConfigured));
    assert_eq!(rig.stack.ping_start(GATEWAY, 1, 0, 0), Err(NetError::NotConfigured));
    let (socket, _) = rig.stack.udp_bind(0, 1).unwrap();
    assert_eq!(rig.stack.udp_send_to(socket, remote, b"x", 0), Err(NetError::NotConfigured));
}

/// Connects a socket, playing the far end by hand.
fn connected(rig: &mut Rig, owner: u32) -> (SocketId, TcpHeader) {
    let remote = Endpoint { addr: GATEWAY, port: 80 };
    let id = rig.stack.tcp_connect(remote, owner, rig.now).unwrap();
    let sent = rig.tcp_sent();
    let (syn, _) = sent.last().expect("a SYN");
    assert_eq!(syn.flags, TCP_SYN);
    assert_eq!(syn.mss, Some(1460));
    assert_eq!(rig.stack.tcp_connected(id), Ok(false));
    let answer = TcpHeader {
        src_port: 80,
        dst_port: syn.src_port,
        seq: 7000,
        ack: syn.seq.wrapping_add(1),
        flags: TCP_SYN | TCP_ACK,
        window: 8192,
        mss: Some(1460),
    };
    rig.deliver(tcp_to_host(GATEWAY, &answer, &[]));
    rig.poll();
    assert_eq!(rig.stack.tcp_connected(id), Ok(true));
    let sent = rig.tcp_sent();
    assert_eq!(sent.last().unwrap().0.flags, TCP_ACK);
    let peer = TcpHeader {
        src_port: 80,
        dst_port: syn.src_port,
        seq: 7001,
        ack: syn.seq.wrapping_add(1),
        flags: TCP_ACK,
        window: 8192,
        mss: None,
    };
    (id, peer)
}

#[test]
fn a_connection_carries_data_both_ways_and_closes() {
    let mut rig = Rig::configured();
    let (id, mut peer) = connected(&mut rig, 1);

    assert_eq!(rig.stack.tcp_send(id, b"hello", rig.now), Ok(Some(5)));
    let sent = rig.tcp_sent();
    assert_eq!(sent[0].1, b"hello");
    peer.ack = peer.ack.wrapping_add(5);
    rig.deliver(tcp_to_host(GATEWAY, &peer, b"world"));
    rig.poll();
    let mut out = [0u8; 16];
    assert_eq!(rig.stack.tcp_recv(id, &mut out, rig.now), Ok(Some(5)));
    assert_eq!(&out[..5], b"world");
    assert_eq!(rig.stack.tcp_recv(id, &mut out, rig.now), Ok(None));

    peer.seq += 5;
    let mut fin = peer;
    fin.flags = TCP_FIN | TCP_ACK;
    rig.deliver(tcp_to_host(GATEWAY, &fin, &[]));
    rig.poll();
    assert_eq!(rig.stack.tcp_recv(id, &mut out, rig.now), Ok(Some(0)));
    assert_eq!(rig.stack.tcp_shutdown(id, rig.now), Ok(()));
    let sent = rig.tcp_sent();
    assert_ne!(sent.last().unwrap().0.flags & TCP_FIN, 0);
    rig.stack.close(id, rig.now);
    // The final ACK ends it.
    peer.seq += 1;
    peer.ack = peer.ack.wrapping_add(1);
    rig.deliver(tcp_to_host(GATEWAY, &peer, &[]));
    rig.poll();
    assert_eq!(rig.stack.stats().tcp_sockets, 0);
    assert_eq!(rig.stack.tcp_recv(id, &mut out, rig.now), Err(NetError::BadSocket));
}

#[test]
fn a_port_nobody_listens_on_answers_with_a_reset() {
    let mut rig = Rig::configured();
    let syn = TcpHeader {
        src_port: 4000,
        dst_port: 9,
        seq: 1234,
        flags: TCP_SYN,
        window: 1000,
        ..TcpHeader::default()
    };
    rig.deliver(tcp_to_host(GATEWAY, &syn, &[]));
    rig.poll();
    let sent = rig.tcp_sent();
    let (reset, _) = sent.last().expect("a reset");
    assert_eq!((reset.flags, reset.ack), (TCP_RST | TCP_ACK, 1235));
}

#[test]
fn a_reset_is_never_answered_with_a_reset() {
    let mut rig = Rig::configured();
    let reset = TcpHeader {
        src_port: 4000,
        dst_port: 9,
        seq: 1234,
        flags: TCP_RST,
        ..TcpHeader::default()
    };
    rig.deliver(tcp_to_host(GATEWAY, &reset, &[]));
    rig.poll();
    assert!(rig.tcp_sent().is_empty());
}

#[test]
fn a_listener_takes_a_connection_and_can_be_closed_with_one_waiting() {
    let mut rig = Rig::configured();
    let listener = rig.stack.tcp_listen(8080, 4, 1).unwrap();
    assert_eq!(rig.stack.tcp_listen(8080, 4, 1), Err(NetError::AddressInUse));
    assert_eq!(rig.stack.tcp_accept(listener, 1), Ok(None));

    let syn = TcpHeader {
        src_port: 4000,
        dst_port: 8080,
        seq: 100,
        flags: TCP_SYN,
        window: 8192,
        mss: Some(1000),
        ..TcpHeader::default()
    };
    rig.deliver(tcp_to_host(GATEWAY, &syn, &[]));
    rig.poll();
    let sent = rig.tcp_sent();
    let (syn_ack, _) = sent.last().unwrap();
    assert_eq!((syn_ack.flags, syn_ack.ack), (TCP_SYN | TCP_ACK, 101));
    assert_eq!(rig.stack.tcp_accept(listener, 1), Ok(None), "not established yet");

    let ack = TcpHeader {
        seq: 101,
        ack: syn_ack.seq.wrapping_add(1),
        flags: TCP_ACK,
        mss: None,
        ..syn
    };
    rig.deliver(tcp_to_host(GATEWAY, &ack, b"early"));
    rig.poll();
    let (child, from) = rig.stack.tcp_accept(listener, 9).unwrap().unwrap();
    assert_eq!(from, Endpoint { addr: GATEWAY, port: 4000 });
    let mut out = [0u8; 16];
    assert_eq!(rig.stack.tcp_recv(child, &mut out, rig.now), Ok(Some(5)));
    assert_eq!(rig.stack.tcp_send(child, &[1; 3000], rig.now), Ok(Some(3000)));
    let sent = rig.tcp_sent();
    assert!(sent.iter().all(|(_, data)| data.len() <= 1000), "their MSS");

    // A second one, which nobody accepts before the listener goes.
    let mut second = syn;
    second.src_port = 4001;
    rig.deliver(tcp_to_host(GATEWAY, &second, &[]));
    rig.poll();
    let sent = rig.tcp_sent();
    let ack = TcpHeader {
        seq: 101,
        ack: sent.last().unwrap().0.seq.wrapping_add(1),
        flags: TCP_ACK,
        mss: None,
        ..second
    };
    rig.deliver(tcp_to_host(GATEWAY, &ack, &[]));
    rig.poll();
    rig.stack.close(listener, rig.now);
    let sent = rig.tcp_sent();
    assert!(sent.iter().any(|(header, _)| header.flags & TCP_RST != 0 && header.dst_port == 4001));
    assert_eq!(rig.stack.stats().tcp_sockets, 1, "only the accepted one");
}

#[test]
fn the_backlog_limits_half_open_connections() {
    let mut rig = Rig::configured();
    rig.stack.tcp_listen(8080, 2, 1).unwrap();
    for port in 0..10 {
        let syn = TcpHeader {
            src_port: 4000 + port,
            dst_port: 8080,
            seq: 100,
            flags: TCP_SYN,
            window: 8192,
            ..TcpHeader::default()
        };
        rig.deliver(tcp_to_host(GATEWAY, &syn, &[]));
        rig.poll();
    }
    assert_eq!(rig.stack.stats().tcp_sockets, 3, "the listener and two");
}

#[test]
fn sockets_run_out_with_an_error_and_a_closed_handle_stays_invalid() {
    let mut rig = Rig::configured();
    let (first, _) = rig.stack.udp_bind(0, 1).unwrap();
    let mut held = vec![first];
    for _ in 1..MAX_SOCKETS {
        held.push(rig.stack.udp_bind(0, 1).unwrap().0);
    }
    assert_eq!(rig.stack.udp_bind(0, 1), Err(NetError::NoResources));
    rig.stack.close(first, rig.now);
    let (reused, _) = rig.stack.udp_bind(0, 1).unwrap();
    assert_eq!(reused.0 & 0xffff, first.0 & 0xffff, "the same slot");
    let mut out = [0u8; 4];
    assert_eq!(rig.stack.udp_recv_from(first, &mut out), Err(NetError::BadSocket));
    assert!(rig.stack.udp_recv_from(reused, &mut out).is_ok());
    assert_eq!(rig.stack.tcp_recv(reused, &mut out, rig.now), Err(NetError::BadSocket));
}

#[test]
fn a_full_device_holds_segments_back_and_they_go_later() {
    let mut rig = Rig::configured();
    let (id, _) = connected(&mut rig, 1);
    rig.device.0.lock().unwrap().full = true;
    assert_eq!(rig.stack.tcp_send(id, b"later", rig.now), Ok(Some(5)));
    assert!(rig.tcp_sent().is_empty());
    let result = rig.poll();
    assert_eq!(result.next_deadline, Some(rig.now + 1));
    rig.device.0.lock().unwrap().full = false;
    rig.now += 1;
    rig.poll();
    assert_eq!(rig.tcp_sent()[0].1, b"later");
}

#[test]
fn a_closed_connection_to_a_vanished_neighbour_gets_freed() {
    let mut rig = Rig::new();
    rig.lease();
    rig.take_frames();
    // Nobody ever answers for this one.
    let id = rig.stack.tcp_connect(Endpoint { addr: Ipv4Addr([10, 0, 2, 77]), port: 80 }, 1, rig.now).unwrap();
    rig.stack.close(id, rig.now);
    assert_eq!(rig.stack.stats().tcp_sockets, 0, "a SYN never sent is nothing to wait for");

    let (id, _) = {
        rig.answer_arp_for(GATEWAY);
        connected(&mut rig, 1)
    };
    // The neighbour goes stale with data still to send, and never answers again.
    rig.now += ARP_TTL_MS + 1;
    assert_eq!(rig.stack.tcp_send(id, b"late", rig.now), Ok(Some(4)));
    let result = rig.poll();
    assert!(result.next_deadline.unwrap() >= rig.now + ARP_RETRY_MS.min(1), "and doesn't spin");
    rig.stack.close(id, rig.now);
    assert_eq!(rig.stack.stats().tcp_sockets, 1);
    rig.now += STRANDED_MS + 1;
    rig.poll();
    rig.now += 1;
    rig.poll();
    assert_eq!(rig.stack.stats().tcp_sockets, 0);
}

#[test]
fn a_dead_owners_sockets_get_closed() {
    let mut rig = Rig::configured();
    let (mine, _) = rig.stack.udp_bind(1000, 5).unwrap();
    let (theirs, _) = rig.stack.udp_bind(1001, 6).unwrap();
    let (tcp, _) = connected(&mut rig, 6);
    rig.stack.close_orphans(|pid| pid == 5, rig.now);
    let mut out = [0u8; 4];
    assert!(rig.stack.udp_recv_from(mine, &mut out).is_ok());
    assert_eq!(rig.stack.udp_recv_from(theirs, &mut out), Err(NetError::BadSocket));
    assert_eq!(rig.stack.tcp_recv(tcp, &mut out, rig.now), Err(NetError::BadSocket));
    assert!(rig.tcp_sent().iter().any(|(header, _)| header.flags & TCP_FIN != 0));
}

#[test]
fn time_wait_lapses_and_makes_way_when_sockets_run_out() {
    let mut rig = Rig::configured();
    let (id, mut peer) = connected(&mut rig, 1);
    rig.stack.close(id, rig.now);
    rig.tcp_sent();
    peer.ack = peer.ack.wrapping_add(1);
    peer.flags = TCP_FIN | TCP_ACK;
    rig.deliver(tcp_to_host(GATEWAY, &peer, &[]));
    rig.poll();
    assert_eq!(rig.stack.stats().tcp_sockets, 1);

    let mut sockets = Vec::new();
    for _ in 1..MAX_SOCKETS {
        sockets.push(rig.stack.udp_bind(0, 1).unwrap());
    }
    assert!(rig.stack.udp_bind(0, 1).is_ok(), "took the slot in TIME-WAIT");
    assert_eq!(rig.stack.stats().tcp_sockets, 0);
}

#[test]
fn a_dns_answer_arrives_through_a_socket() {
    let mut rig = Rig::configured();
    let (socket, port) = rig.stack.udp_bind(0, 1).unwrap();
    let query = dns::build_query(7, "feos.test").unwrap();
    rig.stack.udp_send_to(socket, Endpoint { addr: DNS, port: 53 }, &query, rig.now).unwrap();
    rig.take_frames();
    let mut reply = query.clone();
    reply[2] = 0x81;
    reply[3] = 0x80;
    reply[6..8].copy_from_slice(&1u16.to_be_bytes());
    reply.extend_from_slice(&[0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 1, 2, 3, 4]);
    rig.deliver(udp_to_host(DNS, 53, port, &reply));
    rig.poll();
    let mut out = [0u8; 256];
    let (length, _) = rig.stack.udp_recv_from(socket, &mut out).unwrap().unwrap();
    assert_eq!(
        dns::parse_response(&out[..length], 7, "feos.test"),
        Some(dns::Answer::Address(Ipv4Addr([1, 2, 3, 4])))
    );
}

/// Whatever comes in off the wire, nothing panics, and the stack still works after.
#[test]
fn malformed_frames_never_panic() {
    let mut rig = Rig::configured();
    let (id, peer) = connected(&mut rig, 1);
    rig.stack.udp_bind(5000, 1).unwrap();
    rig.stack.tcp_listen(8080, 8, 1).unwrap();

    let mut state = 0xdead_beef_u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let seeds: Vec<Vec<u8>> = vec![
        udp_to_host(GATEWAY, 7, 5000, b"some data"),
        udp_to_host(GATEWAY, 67, 68, &dhcp_reply(&[0u8; 64], 5)),
        tcp_to_host(GATEWAY, &peer, b"hello"),
        tcp_to_host(GATEWAY, &TcpHeader { src_port: 1, dst_port: 8080, flags: TCP_SYN, mss: Some(1), ..TcpHeader::default() }, &[]),
        frame_to_host(GATEWAY, PROTO_ICMP, &[8, 0, 0, 0, 0, 1, 0, 2, 9]),
    ];
    for round in 0..100_000usize {
        let mut frame = seeds[round % seeds.len()].clone();
        let flips = round % 6;
        for _ in 0..flips {
            let at = next() as usize % frame.len();
            frame[at] = next() as u8;
        }
        if round % 7 == 0 {
            frame.truncate(next() as usize % (frame.len() + 1));
        }
        if round % 11 == 0 {
            frame = (0..next() as usize % 80).map(|_| next() as u8).collect();
            if frame.len() >= 6 {
                frame[..6].copy_from_slice(&MAC);
            }
        }
        rig.stack.input(&frame, rig.now);
        if round % 500 == 0 {
            rig.now += 100;
            rig.poll();
            rig.take_frames();
        }
    }
    // Still answers.
    let _ = id;
    let mut echo = [0u8; 16];
    let length = write_icmp_echo(&mut echo, ICMP_ECHO_REQUEST, 1, 1, b"up");
    rig.take_frames();
    rig.deliver(frame_to_host(GATEWAY, PROTO_ICMP, &echo[..length]));
    rig.poll();
    assert!(!rig.take_frames().is_empty());
}
