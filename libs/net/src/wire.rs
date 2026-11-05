//! Packet formats: parsing with every length checked, and building.
//!
//! A parser takes bytes that anyone on the network could have made up. It returns
//! `None` for anything it can't use and never panics.

use feos_hal::net::Ipv4Addr;

/// Bytes in an Ethernet header.
pub const ETH_HEADER: usize = 14;
/// Largest IP packet this host sends or takes.
pub const MTU: usize = 1500;
/// Largest frame, without a frame check sequence.
pub const MAX_FRAME: usize = ETH_HEADER + MTU;
/// Bytes in an IPv4 header without options, which is all this host sends.
pub const IP_HEADER: usize = 20;
/// Bytes in a UDP header.
pub const UDP_HEADER: usize = 8;
/// Bytes in a TCP header without options.
pub const TCP_HEADER: usize = 20;
/// Bytes in an ICMP echo header.
pub const ICMP_HEADER: usize = 8;

/// EtherType of IPv4.
pub const ETHERTYPE_IPV4: u16 = 0x0800;
/// EtherType of ARP.
pub const ETHERTYPE_ARP: u16 = 0x0806;
/// The broadcast hardware address.
pub const MAC_BROADCAST: [u8; 6] = [0xff; 6];

/// IP protocol number of ICMP.
pub const PROTO_ICMP: u8 = 1;
/// IP protocol number of TCP.
pub const PROTO_TCP: u8 = 6;
/// IP protocol number of UDP.
pub const PROTO_UDP: u8 = 17;

/// TCP flag: no more data from the sender.
pub const TCP_FIN: u8 = 0x01;
/// TCP flag: synchronize sequence numbers.
pub const TCP_SYN: u8 = 0x02;
/// TCP flag: reset the connection.
pub const TCP_RST: u8 = 0x04;
/// TCP flag: push.
pub const TCP_PSH: u8 = 0x08;
/// TCP flag: the acknowledgment field is significant.
pub const TCP_ACK: u8 = 0x10;

fn be16(data: &[u8], at: usize) -> Option<u16> {
    let bytes = data.get(at..at.checked_add(2)?)?;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn be32(data: &[u8], at: usize) -> Option<u32> {
    let bytes = data.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn addr(data: &[u8], at: usize) -> Option<Ipv4Addr> {
    let bytes = data.get(at..at.checked_add(4)?)?;
    Some(Ipv4Addr([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn mac(data: &[u8], at: usize) -> Option<[u8; 6]> {
    let bytes = data.get(at..at.checked_add(6)?)?;
    let mut out = [0u8; 6];
    out.copy_from_slice(bytes);
    Some(out)
}

/// The internet checksum's running sum over `data`, on top of `sum`.
pub fn checksum_add(mut sum: u32, data: &[u8]) -> u32 {
    let mut chunks = data.chunks_exact(2);
    for chunk in &mut chunks {
        sum += u32::from(u16::from_be_bytes([chunk[0], chunk[1]]));
        // Folding as it goes keeps this from overflowing on any length.
        sum = (sum & 0xffff) + (sum >> 16);
    }
    if let [last] = chunks.remainder() {
        sum += u32::from(*last) << 8;
    }
    sum
}

/// Finishes a running sum.
pub fn checksum_finish(mut sum: u32) -> u16 {
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// The internet checksum of `data`.
pub fn checksum(data: &[u8]) -> u16 {
    checksum_finish(checksum_add(0, data))
}

fn pseudo_header(src: Ipv4Addr, dst: Ipv4Addr, protocol: u8, length: usize) -> u32 {
    let mut sum = checksum_add(0, &src.0);
    sum = checksum_add(sum, &dst.0);
    sum += u32::from(protocol);
    sum + length as u32
}

/// A parsed Ethernet header and what it carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ethernet<'a> {
    /// Destination hardware address.
    pub dst: [u8; 6],
    /// Source hardware address.
    pub src: [u8; 6],
    /// EtherType.
    pub ethertype: u16,
    /// Everything after the header.
    pub payload: &'a [u8],
}

/// Parses an Ethernet frame.
pub fn parse_ethernet(frame: &[u8]) -> Option<Ethernet<'_>> {
    Some(Ethernet {
        dst: mac(frame, 0)?,
        src: mac(frame, 6)?,
        ethertype: be16(frame, 12)?,
        payload: frame.get(ETH_HEADER..)?,
    })
}

/// Writes an Ethernet header at the start of `out`.
pub fn write_ethernet(out: &mut [u8], dst: [u8; 6], src: [u8; 6], ethertype: u16) {
    out[0..6].copy_from_slice(&dst);
    out[6..12].copy_from_slice(&src);
    out[12..14].copy_from_slice(&ethertype.to_be_bytes());
}

/// ARP's request code.
pub const ARP_REQUEST: u16 = 1;
/// ARP's reply code.
pub const ARP_REPLY: u16 = 2;
/// Bytes in an ARP packet for IPv4 over Ethernet.
pub const ARP_LEN: usize = 28;

/// A parsed ARP packet for IPv4 over Ethernet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arp {
    /// Request or reply.
    pub operation: u16,
    /// Sender's hardware address.
    pub sender_mac: [u8; 6],
    /// Sender's IP address.
    pub sender_ip: Ipv4Addr,
    /// Target's hardware address.
    pub target_mac: [u8; 6],
    /// Target's IP address.
    pub target_ip: Ipv4Addr,
}

/// Parses an ARP packet, of the one kind this stack has any use for.
pub fn parse_arp(data: &[u8]) -> Option<Arp> {
    if be16(data, 0)? != 1 || be16(data, 2)? != ETHERTYPE_IPV4 || *data.get(4)? != 6 || *data.get(5)? != 4 {
        return None;
    }
    Some(Arp {
        operation: be16(data, 6)?,
        sender_mac: mac(data, 8)?,
        sender_ip: addr(data, 14)?,
        target_mac: mac(data, 18)?,
        target_ip: addr(data, 24)?,
    })
}

/// Writes an ARP packet at the start of `out`, which must have [`ARP_LEN`] bytes.
pub fn write_arp(out: &mut [u8], packet: &Arp) {
    out[0..2].copy_from_slice(&1u16.to_be_bytes());
    out[2..4].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
    out[4] = 6;
    out[5] = 4;
    out[6..8].copy_from_slice(&packet.operation.to_be_bytes());
    out[8..14].copy_from_slice(&packet.sender_mac);
    out[14..18].copy_from_slice(&packet.sender_ip.0);
    out[18..24].copy_from_slice(&packet.target_mac);
    out[24..28].copy_from_slice(&packet.target_ip.0);
}

/// A parsed IPv4 packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipv4<'a> {
    /// Source address.
    pub src: Ipv4Addr,
    /// Destination address.
    pub dst: Ipv4Addr,
    /// Protocol number.
    pub protocol: u8,
    /// Time to live.
    pub ttl: u8,
    /// What it carries, cut to the length the header claims. Link padding is gone.
    pub payload: &'a [u8],
}

/// Parses an IPv4 packet and checks its header. A fragment is `None`: this host
/// doesn't reassemble them, and sends with Don't Fragment set.
pub fn parse_ipv4(data: &[u8]) -> Option<Ipv4<'_>> {
    let first = *data.first()?;
    if first >> 4 != 4 {
        return None;
    }
    let header = usize::from(first & 0xf) * 4;
    if header < IP_HEADER {
        return None;
    }
    let total = usize::from(be16(data, 2)?);
    if total < header || total > data.len() {
        return None;
    }
    let head = data.get(..header)?;
    if checksum(head) != 0 {
        return None;
    }
    let fragment = be16(data, 6)?;
    // More Fragments, or any offset.
    if fragment & 0x3fff != 0 {
        return None;
    }
    Some(Ipv4 {
        src: addr(data, 12)?,
        dst: addr(data, 16)?,
        protocol: *data.get(9)?,
        ttl: *data.get(8)?,
        payload: data.get(header..total)?,
    })
}

/// Writes an IPv4 header for a `payload`-byte packet at the start of `out`.
pub fn write_ipv4(out: &mut [u8], src: Ipv4Addr, dst: Ipv4Addr, protocol: u8, payload: usize, id: u16) {
    let head = &mut out[..IP_HEADER];
    head[0] = 0x45;
    head[1] = 0;
    head[2..4].copy_from_slice(&((IP_HEADER + payload) as u16).to_be_bytes());
    head[4..6].copy_from_slice(&id.to_be_bytes());
    // Don't Fragment.
    head[6..8].copy_from_slice(&0x4000u16.to_be_bytes());
    head[8] = 64;
    head[9] = protocol;
    head[10..12].copy_from_slice(&[0, 0]);
    head[12..16].copy_from_slice(&src.0);
    head[16..20].copy_from_slice(&dst.0);
    let sum = checksum(head);
    head[10..12].copy_from_slice(&sum.to_be_bytes());
}

/// ICMP's echo reply type.
pub const ICMP_ECHO_REPLY: u8 = 0;
/// ICMP's echo request type.
pub const ICMP_ECHO_REQUEST: u8 = 8;

/// A parsed ICMP echo message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IcmpEcho<'a> {
    /// Request or reply.
    pub kind: u8,
    /// Identifier.
    pub id: u16,
    /// Sequence number.
    pub sequence: u16,
    /// Whatever was sent to be echoed.
    pub data: &'a [u8],
}

/// Parses an ICMP echo request or reply and checks its checksum. Any other ICMP
/// message is `None`.
pub fn parse_icmp_echo(data: &[u8]) -> Option<IcmpEcho<'_>> {
    if data.len() < ICMP_HEADER || checksum(data) != 0 {
        return None;
    }
    let kind = *data.first()?;
    if !(kind == ICMP_ECHO_REQUEST || kind == ICMP_ECHO_REPLY) || *data.get(1)? != 0 {
        return None;
    }
    Some(IcmpEcho {
        kind,
        id: be16(data, 4)?,
        sequence: be16(data, 6)?,
        data: data.get(ICMP_HEADER..)?,
    })
}

/// Writes an ICMP echo message at the start of `out` and returns its length.
pub fn write_icmp_echo(out: &mut [u8], kind: u8, id: u16, sequence: u16, data: &[u8]) -> usize {
    let length = ICMP_HEADER + data.len();
    let message = &mut out[..length];
    message[0] = kind;
    message[1] = 0;
    message[2..4].copy_from_slice(&[0, 0]);
    message[4..6].copy_from_slice(&id.to_be_bytes());
    message[6..8].copy_from_slice(&sequence.to_be_bytes());
    message[8..].copy_from_slice(data);
    let sum = checksum(message);
    message[2..4].copy_from_slice(&sum.to_be_bytes());
    length
}

/// A parsed UDP datagram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Udp<'a> {
    /// Source port.
    pub src_port: u16,
    /// Destination port.
    pub dst_port: u16,
    /// The datagram, cut to the length the header claims.
    pub payload: &'a [u8],
}

/// Parses a UDP datagram and checks its checksum, if it has one.
pub fn parse_udp(src: Ipv4Addr, dst: Ipv4Addr, data: &[u8]) -> Option<Udp<'_>> {
    let length = usize::from(be16(data, 4)?);
    if length < UDP_HEADER || length > data.len() {
        return None;
    }
    let datagram = data.get(..length)?;
    if be16(data, 6)? != 0 {
        let sum = checksum_add(pseudo_header(src, dst, PROTO_UDP, length), datagram);
        if checksum_finish(sum) != 0 {
            return None;
        }
    }
    Some(Udp {
        src_port: be16(data, 0)?,
        dst_port: be16(data, 2)?,
        payload: datagram.get(UDP_HEADER..)?,
    })
}

/// Writes a UDP datagram at the start of `out` and returns its length.
pub fn write_udp(out: &mut [u8], src: (Ipv4Addr, u16), dst: (Ipv4Addr, u16), data: &[u8]) -> usize {
    let length = UDP_HEADER + data.len();
    let datagram = &mut out[..length];
    datagram[0..2].copy_from_slice(&src.1.to_be_bytes());
    datagram[2..4].copy_from_slice(&dst.1.to_be_bytes());
    datagram[4..6].copy_from_slice(&(length as u16).to_be_bytes());
    datagram[6..8].copy_from_slice(&[0, 0]);
    datagram[8..].copy_from_slice(data);
    let mut sum = checksum_finish(checksum_add(pseudo_header(src.0, dst.0, PROTO_UDP, length), datagram));
    // Zero means "no checksum", and an all-ones sum means the same arithmetic.
    if sum == 0 {
        sum = 0xffff;
    }
    datagram[6..8].copy_from_slice(&sum.to_be_bytes());
    length
}

/// A parsed TCP segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tcp<'a> {
    /// Source port.
    pub src_port: u16,
    /// Destination port.
    pub dst_port: u16,
    /// Sequence number.
    pub seq: u32,
    /// Acknowledgment number.
    pub ack: u32,
    /// Flags.
    pub flags: u8,
    /// Advertised window.
    pub window: u16,
    /// Maximum segment size option.
    pub mss: Option<u16>,
    /// The data.
    pub payload: &'a [u8],
}

/// Parses a TCP segment and checks its checksum.
pub fn parse_tcp(src: Ipv4Addr, dst: Ipv4Addr, data: &[u8]) -> Option<Tcp<'_>> {
    if data.len() < TCP_HEADER {
        return None;
    }
    let offset = usize::from(*data.get(12)? >> 4) * 4;
    if offset < TCP_HEADER || offset > data.len() {
        return None;
    }
    let sum = checksum_add(pseudo_header(src, dst, PROTO_TCP, data.len()), data);
    if checksum_finish(sum) != 0 {
        return None;
    }
    Some(Tcp {
        src_port: be16(data, 0)?,
        dst_port: be16(data, 2)?,
        seq: be32(data, 4)?,
        ack: be32(data, 8)?,
        flags: *data.get(13)?,
        window: be16(data, 14)?,
        mss: tcp_mss_option(data.get(TCP_HEADER..offset)?),
        payload: data.get(offset..)?,
    })
}

/// Finds the MSS option. A malformed list just ends the search.
fn tcp_mss_option(mut options: &[u8]) -> Option<u16> {
    loop {
        match *options.first()? {
            0 => return None,
            1 => options = options.get(1..)?,
            kind => {
                let length = usize::from(*options.get(1)?);
                if length < 2 {
                    return None;
                }
                if kind == 2 && length == 4 {
                    return be16(options, 2);
                }
                options = options.get(length..)?;
            }
        }
    }
}

/// The fields of a TCP header to be written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct TcpHeader {
    /// Source port.
    pub src_port: u16,
    /// Destination port.
    pub dst_port: u16,
    /// Sequence number.
    pub seq: u32,
    /// Acknowledgment number.
    pub ack: u32,
    /// Flags.
    pub flags: u8,
    /// Advertised window.
    pub window: u16,
    /// Maximum segment size option.
    pub mss: Option<u16>,
}

/// Writes a TCP segment, whose `len` bytes of data are already in `out` where
/// [`tcp_data_offset`] says, and returns the segment's length.
pub fn write_tcp(out: &mut [u8], src: Ipv4Addr, dst: Ipv4Addr, header: &TcpHeader, len: usize) -> usize {
    let offset = tcp_data_offset(header.mss.is_some());
    let total = offset + len;
    let segment = &mut out[..total];
    segment[0..2].copy_from_slice(&header.src_port.to_be_bytes());
    segment[2..4].copy_from_slice(&header.dst_port.to_be_bytes());
    segment[4..8].copy_from_slice(&header.seq.to_be_bytes());
    segment[8..12].copy_from_slice(&header.ack.to_be_bytes());
    segment[12] = (offset as u8 / 4) << 4;
    segment[13] = header.flags;
    segment[14..16].copy_from_slice(&header.window.to_be_bytes());
    segment[16..20].copy_from_slice(&[0; 4]);
    if let Some(mss) = header.mss {
        segment[20] = 2;
        segment[21] = 4;
        segment[22..24].copy_from_slice(&mss.to_be_bytes());
    }
    let sum = checksum_finish(checksum_add(pseudo_header(src, dst, PROTO_TCP, total), segment));
    segment[16..18].copy_from_slice(&sum.to_be_bytes());
    total
}

/// Where a TCP segment's data starts.
pub const fn tcp_data_offset(has_mss: bool) -> usize {
    if has_mss {
        TCP_HEADER + 4
    } else {
        TCP_HEADER
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Ipv4Addr = Ipv4Addr([10, 0, 2, 15]);
    const B: Ipv4Addr = Ipv4Addr([10, 0, 2, 2]);

    /// A deterministic stream of bytes.
    fn noise(seed: u64) -> impl FnMut() -> u8 {
        let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        }
    }

    #[test]
    fn the_checksum_matches_the_rfc_1071_example() {
        let data = [0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];
        assert_eq!(checksum(&data), !0xddf2);
    }

    #[test]
    fn an_odd_length_pads_with_zero() {
        assert_eq!(checksum(&[0xab]), !0xab00);
    }

    #[test]
    fn a_huge_buffer_of_ones_does_not_overflow_the_sum() {
        let data = vec![0xffu8; 1 << 20];
        assert_eq!(checksum(&data), 0);
    }

    #[test]
    fn ipv4_round_trips_and_a_bad_header_is_refused() {
        let mut packet = [0u8; 40];
        write_ipv4(&mut packet, A, B, PROTO_UDP, 20, 7);
        let parsed = parse_ipv4(&packet).unwrap();
        assert_eq!((parsed.src, parsed.dst, parsed.protocol), (A, B, PROTO_UDP));
        assert_eq!(parsed.payload.len(), 20);
        packet[15] ^= 1;
        assert!(parse_ipv4(&packet).is_none());
    }

    #[test]
    fn ipv4_fragments_are_refused_and_padding_is_cut() {
        let mut packet = [0u8; 64];
        write_ipv4(&mut packet, A, B, PROTO_UDP, 4, 7);
        assert_eq!(parse_ipv4(&packet).unwrap().payload.len(), 4);
        // More Fragments set, checksum redone.
        packet[6..8].copy_from_slice(&0x2000u16.to_be_bytes());
        packet[10..12].copy_from_slice(&[0, 0]);
        let sum = checksum(&packet[..20]);
        packet[10..12].copy_from_slice(&sum.to_be_bytes());
        assert!(parse_ipv4(&packet).is_none());
    }

    #[test]
    fn ipv4_header_lengths_that_lie_are_refused() {
        let mut packet = [0u8; 40];
        write_ipv4(&mut packet, A, B, PROTO_UDP, 20, 7);
        for ihl in [0u8, 4, 15] {
            let mut bad = packet;
            bad[0] = 0x40 | ihl;
            assert!(parse_ipv4(&bad[..24]).is_none());
        }
        let mut bad = packet;
        bad[2..4].copy_from_slice(&5000u16.to_be_bytes());
        assert!(parse_ipv4(&bad).is_none());
    }

    #[test]
    fn udp_round_trips_and_a_corrupted_one_is_refused() {
        let mut datagram = [0u8; 64];
        let length = write_udp(&mut datagram, (A, 1000), (B, 53), b"hello");
        let parsed = parse_udp(A, B, &datagram[..length]).unwrap();
        assert_eq!((parsed.src_port, parsed.dst_port, parsed.payload), (1000, 53, &b"hello"[..]));
        datagram[9] ^= 1;
        assert!(parse_udp(A, B, &datagram[..length]).is_none());
    }

    #[test]
    fn udp_without_a_checksum_is_taken() {
        let datagram = [0x03, 0xe8, 0x00, 0x35, 0x00, 0x09, 0, 0, b'x'];
        assert_eq!(parse_udp(A, B, &datagram).unwrap().payload, b"x");
    }

    #[test]
    fn tcp_round_trips_with_its_option() {
        let mut segment = [0u8; 64];
        let offset = tcp_data_offset(true);
        segment[offset..offset + 3].copy_from_slice(b"abc");
        let header = TcpHeader {
            src_port: 1,
            dst_port: 2,
            seq: 3,
            ack: 4,
            flags: TCP_SYN | TCP_ACK,
            window: 5,
            mss: Some(1460),
        };
        let length = write_tcp(&mut segment, A, B, &header, 3);
        let parsed = parse_tcp(A, B, &segment[..length]).unwrap();
        assert_eq!(parsed.mss, Some(1460));
        assert_eq!(parsed.payload, b"abc");
        assert_eq!((parsed.seq, parsed.ack, parsed.window), (3, 4, 5));
        assert!(parse_tcp(B, A, &segment[..length]).is_some(), "the sum doesn't care for order");
        assert!(parse_tcp(A, Ipv4Addr([1, 1, 1, 1]), &segment[..length]).is_none());
    }

    #[test]
    fn tcp_options_that_lie_end_the_search() {
        assert_eq!(tcp_mss_option(&[1, 1, 2, 4, 5, 0xb4]), Some(1460));
        assert_eq!(tcp_mss_option(&[0, 2, 4, 5, 0xb4]), None);
        assert_eq!(tcp_mss_option(&[8, 0, 2, 4, 5, 0xb4]), None);
        assert_eq!(tcp_mss_option(&[8, 200]), None);
        assert_eq!(tcp_mss_option(&[2, 4, 5]), None);
        assert_eq!(tcp_mss_option(&[2]), None);
        assert_eq!(tcp_mss_option(&[]), None);
    }

    #[test]
    fn icmp_round_trips() {
        let mut message = [0u8; 32];
        let length = write_icmp_echo(&mut message, ICMP_ECHO_REQUEST, 9, 10, b"ping");
        let parsed = parse_icmp_echo(&message[..length]).unwrap();
        assert_eq!((parsed.id, parsed.sequence, parsed.data), (9, 10, &b"ping"[..]));
        message[9] ^= 1;
        assert!(parse_icmp_echo(&message[..length]).is_none());
    }

    #[test]
    fn arp_round_trips() {
        let packet = Arp {
            operation: ARP_REQUEST,
            sender_mac: [1; 6],
            sender_ip: A,
            target_mac: [0; 6],
            target_ip: B,
        };
        let mut bytes = [0u8; ARP_LEN];
        write_arp(&mut bytes, &packet);
        assert_eq!(parse_arp(&bytes), Some(packet));
        assert_eq!(parse_arp(&bytes[..27]), None);
        bytes[4] = 8;
        assert_eq!(parse_arp(&bytes), None);
    }

    #[test]
    fn dotted_quads_parse_strictly() {
        assert_eq!(Ipv4Addr::parse("10.0.2.15"), Some(A));
        for bad in ["", "1.2.3", "1.2.3.4.5", "256.1.1.1", "a.b.c.d", "1..2.3", " 1.2.3.4", "1.2.3.4 ", "+1.2.3.4", "0001.2.3.4"] {
            assert_eq!(Ipv4Addr::parse(bad), None, "{bad:?}");
        }
    }

    /// Every parser, over every prefix of a good packet with bytes flipped and over
    /// pure noise. Nothing may panic: that's the whole of what's being tested.
    #[test]
    fn no_parser_panics_on_any_input() {
        let mut packet = [0u8; 80];
        let offset = IP_HEADER + tcp_data_offset(true);
        packet[offset..offset + 4].copy_from_slice(b"data");
        write_tcp(
            &mut packet[IP_HEADER..],
            A,
            B,
            &TcpHeader {
                flags: TCP_SYN,
                mss: Some(1400),
                ..TcpHeader::default()
            },
            4,
        );
        write_ipv4(&mut packet, A, B, PROTO_TCP, 28, 1);
        let mut next = noise(7);
        for round in 0..200_000usize {
            let mut bytes = packet;
            let length = round % (bytes.len() + 1);
            if round % 3 == 0 {
                for byte in bytes.iter_mut() {
                    *byte = next();
                }
            } else {
                for _ in 0..1 + round % 4 {
                    let at = usize::from(next()) % bytes.len();
                    bytes[at] = next();
                }
            }
            let data = &bytes[..length];
            let _ = parse_ethernet(data);
            let _ = parse_arp(data);
            if let Some(ip) = parse_ipv4(data) {
                let _ = parse_tcp(ip.src, ip.dst, ip.payload);
                let _ = parse_udp(ip.src, ip.dst, ip.payload);
                let _ = parse_icmp_echo(ip.payload);
            }
            let _ = parse_tcp(A, B, data);
            let _ = parse_udp(A, B, data);
            let _ = parse_icmp_echo(data);
            let _ = tcp_mss_option(data);
        }
    }
}
