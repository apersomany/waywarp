use std::net::{Ipv4Addr, SocketAddrV4};

pub const HEADERS: usize = 28;

pub struct Outbound {
    pub source: SocketAddrV4,
    pub destination: SocketAddrV4,
    pub payload: std::ops::Range<usize>,
}

fn checksum(bytes: &[u8]) -> u16 {
    let mut sum: u32 = bytes
        .chunks(2)
        .map(|pair| u32::from(u16::from_be_bytes([pair[0], *pair.get(1).unwrap_or(&0)])))
        .sum();
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn unicast(address: Ipv4Addr) -> bool {
    !address.is_unspecified() && !address.is_multicast() && !address.is_broadcast()
}

// Accepts unfragmented IPv4 unicast UDP; checksums are skipped because the local kernel wrote the packet.
pub fn outbound(packet: &[u8]) -> Option<Outbound> {
    if packet.len() < HEADERS || packet[0] >> 4 != 4 || packet[9] != 17 {
        return None;
    }
    let header = usize::from(packet[0] & 15) * 4;
    let total = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    let fragmented = u16::from_be_bytes([packet[6], packet[7]]) & 0x3fff != 0;
    if header < 20 || total != packet.len() || total < header + 8 || fragmented {
        return None;
    }
    let udp = &packet[header..];
    let source = Ipv4Addr::new(packet[12], packet[13], packet[14], packet[15]);
    let destination = Ipv4Addr::new(packet[16], packet[17], packet[18], packet[19]);
    let source_port = u16::from_be_bytes([udp[0], udp[1]]);
    let destination_port = u16::from_be_bytes([udp[2], udp[3]]);
    let length = usize::from(u16::from_be_bytes([udp[4], udp[5]]));
    if !unicast(source)
        || !unicast(destination)
        || source_port == 0
        || destination_port == 0
        || length != udp.len()
    {
        return None;
    }
    Some(Outbound {
        source: SocketAddrV4::new(source, source_port),
        destination: SocketAddrV4::new(destination, destination_port),
        payload: header + 8..packet.len(),
    })
}

// Writes IPv4 and UDP headers in front of a payload already placed at buffer[HEADERS..].
pub fn inbound(
    buffer: &mut [u8],
    payload: usize,
    source: SocketAddrV4,
    destination: SocketAddrV4,
) -> &[u8] {
    let total = HEADERS + payload;
    let (ip, rest) = buffer[..total].split_at_mut(20);
    ip.copy_from_slice(&[
        0x45, 0, 0, 0, 0, 0, 0x40, 0, 64, 17, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ]);
    ip[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    ip[12..16].copy_from_slice(&source.ip().octets());
    ip[16..20].copy_from_slice(&destination.ip().octets());
    let sum = checksum(ip);
    ip[10..12].copy_from_slice(&sum.to_be_bytes());
    // A zero UDP checksum means "not computed", which IPv4 permits.
    rest[0..2].copy_from_slice(&source.port().to_be_bytes());
    rest[2..4].copy_from_slice(&destination.port().to_be_bytes());
    rest[4..6].copy_from_slice(&((payload + 8) as u16).to_be_bytes());
    rest[6..8].fill(0);
    &buffer[..total]
}

// Payloads larger than this cannot be framed into a single IPv4 packet.
pub const MAX_PAYLOAD: usize = u16::MAX as usize - HEADERS;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbound_packets_parse_back_and_fragments_are_rejected() {
        let source = SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 4), 443);
        let destination = SocketAddrV4::new(Ipv4Addr::new(10, 79, 0, 2), 50000);
        let mut buffer = vec![0; 128];
        buffer[HEADERS..HEADERS + 5].copy_from_slice(b"hello");
        let mut packet = inbound(&mut buffer, 5, source, destination).to_vec();
        assert_eq!(checksum(&packet[..20]), 0);
        let parsed = outbound(&packet).unwrap();
        assert_eq!((parsed.source, parsed.destination), (source, destination));
        assert_eq!(&packet[parsed.payload], b"hello");
        packet[6] = 0x20;
        assert!(outbound(&packet).is_none());
    }
}
