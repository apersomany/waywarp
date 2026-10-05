use std::collections::VecDeque;

const VERSION_1: u32 = 1;
const VERSION_2: u32 = 0x6b33_43cf;
const MAX_CID_LENGTH: usize = 20;
const MAX_ATTEMPTS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Initial,
    ZeroRtt,
    Handshake,
    Retry,
    VersionNegotiation,
    OneRtt,
}

#[derive(Debug)]
struct Packet<'a> {
    kind: Kind,
    destination: &'a [u8],
    source: &'a [u8],
    bytes: &'a [u8],
}

fn take<'a>(bytes: &mut &'a [u8], length: usize) -> Option<&'a [u8]> {
    let value = bytes.get(..length)?;
    *bytes = &bytes[length..];
    Some(value)
}

fn varint(bytes: &mut &[u8]) -> Option<usize> {
    let length = 1 << (bytes.first()? >> 6);
    let encoded = take(bytes, length)?;
    let mut value = u64::from(encoded[0] & 0x3f);
    for byte in &encoded[1..] {
        value = (value << 8) | u64::from(*byte);
    }
    usize::try_from(value).ok()
}

fn connection_id<'a>(bytes: &mut &'a [u8]) -> Option<&'a [u8]> {
    let length = usize::from(take(bytes, 1)?[0]);
    if length > MAX_CID_LENGTH {
        return None;
    }
    take(bytes, length)
}

struct Packets<'a>(&'a [u8]);

impl<'a> Iterator for Packets<'a> {
    type Item = Packet<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let datagram = std::mem::take(&mut self.0);
        let mut remaining = datagram;
        let first = take(&mut remaining, 1)?[0];
        if first & 0x80 == 0 {
            // The fixed bit may be greased. The CID length and protected packet number are
            // deliberately not guessed; a known CID is matched by the observer instead.
            return (datagram.len() >= 21).then_some(Packet {
                kind: Kind::OneRtt,
                destination: &[],
                source: &[],
                bytes: datagram,
            });
        }
        let version = u32::from_be_bytes(take(&mut remaining, 4)?.try_into().ok()?);
        let destination = connection_id(&mut remaining)?;
        let source = connection_id(&mut remaining)?;
        let kind = match (version, (first >> 4) & 3) {
            (0, _) if !remaining.is_empty() && remaining.len().is_multiple_of(4) => {
                Kind::VersionNegotiation
            }
            (VERSION_1, 0) | (VERSION_2, 1) => Kind::Initial,
            (VERSION_1, 1) | (VERSION_2, 2) => Kind::ZeroRtt,
            (VERSION_1, 2) | (VERSION_2, 3) => Kind::Handshake,
            (VERSION_1, 3) | (VERSION_2, 0) => Kind::Retry,
            _ => return None,
        };
        if version != 0 && first & 0x40 == 0 {
            return None;
        }
        if kind == Kind::Initial {
            let token_length = varint(&mut remaining)?;
            take(&mut remaining, token_length)?;
        }
        let length = match kind {
            Kind::VersionNegotiation => datagram.len(),
            Kind::Retry if remaining.len() >= 16 => datagram.len(),
            Kind::Retry => return None,
            _ => {
                let protected_length = varint(&mut remaining)?;
                // Header protection needs a sample after four possible packet-number bytes.
                if protected_length < 20 {
                    return None;
                }
                take(&mut remaining, protected_length)?;
                datagram.len() - remaining.len()
            }
        };
        self.0 = &datagram[length..];
        Some(Packet {
            kind,
            destination,
            source,
            bytes: &datagram[..length],
        })
    }
}

#[derive(Default)]
struct Attempt {
    client: Vec<u8>,
    initial_destination: Vec<u8>,
    server: Option<Vec<u8>>,
    progressed: bool,
    client_one_rtt: bool,
    server_one_rtt: bool,
    exchanged: bool,
}

impl Attempt {
    fn initial_matches(&self, packet: &Packet<'_>) -> bool {
        !self.progressed
            && self.client == packet.source
            && (!self.client.is_empty()
                || self.initial_destination == packet.destination
                || self.server.as_deref() == Some(packet.destination))
    }

    fn client_matches(&self, packet: &Packet<'_>) -> bool {
        self.client == packet.source
            && (!self.client.is_empty()
                || self.server.as_deref() == Some(packet.destination)
                || self.initial_destination == packet.destination)
    }

    fn exchange(&mut self, change: &mut Change) {
        if !self.exchanged && self.client_one_rtt && self.server_one_rtt {
            self.exchanged = true;
            change.exchanges += 1;
        }
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
pub(super) struct Change {
    pub attempts: u64,
    pub exchanges: u64,
}

#[derive(Default)]
pub(super) struct Connection {
    attempts: VecDeque<Attempt>,
}

fn short_matches(packet: &Packet<'_>, connection: &[u8]) -> bool {
    packet.bytes.len() >= 21 + connection.len()
        && packet.bytes.get(1..1 + connection.len()) == Some(connection)
}

impl Connection {
    pub fn uplink(&mut self, datagram: &[u8]) -> Change {
        let mut change = Change::default();
        for packet in Packets(datagram) {
            match packet.kind {
                Kind::Initial => {
                    if self
                        .attempts
                        .iter()
                        .rev()
                        .any(|attempt| attempt.initial_matches(&packet))
                    {
                        continue;
                    }
                    if self.attempts.len() == MAX_ATTEMPTS {
                        self.attempts.pop_front();
                    }
                    self.attempts.push_back(Attempt {
                        client: packet.source.to_vec(),
                        initial_destination: packet.destination.to_vec(),
                        ..Attempt::default()
                    });
                    change.attempts += 1;
                }
                Kind::Handshake => {
                    if let Some(attempt) = self
                        .attempts
                        .iter_mut()
                        .rev()
                        .find(|attempt| attempt.client_matches(&packet))
                    {
                        attempt.server = Some(packet.destination.to_vec());
                        attempt.progressed = true;
                    }
                }
                Kind::OneRtt => {
                    if let Some(attempt) = self.attempts.iter_mut().rev().find(|attempt| {
                        attempt
                            .server
                            .as_deref()
                            .is_some_and(|server| short_matches(&packet, server))
                    }) {
                        attempt.progressed = true;
                        attempt.client_one_rtt = true;
                        attempt.exchange(&mut change);
                    }
                }
                _ => {}
            }
        }
        change
    }

    pub fn downlink(&mut self, datagram: &[u8]) -> Change {
        let mut change = Change::default();
        for packet in Packets(datagram) {
            match packet.kind {
                Kind::Initial | Kind::Handshake | Kind::Retry => {
                    if let Some(attempt) = self
                        .attempts
                        .iter_mut()
                        .rev()
                        .find(|attempt| attempt.client == packet.destination)
                    {
                        attempt.server = Some(packet.source.to_vec());
                    }
                }
                Kind::OneRtt => {
                    if let Some(attempt) = self
                        .attempts
                        .iter_mut()
                        .rev()
                        .find(|attempt| short_matches(&packet, &attempt.client))
                    {
                        attempt.server_one_rtt = true;
                        attempt.exchange(&mut change);
                    }
                }
                _ => {}
            }
        }
        change
    }
}

#[cfg(test)]
pub(super) mod fixtures {
    pub fn long(version: u32, kind: u8, destination: &[u8], source: &[u8]) -> Vec<u8> {
        let mut packet = vec![0xc0 | (kind << 4)];
        packet.extend_from_slice(&version.to_be_bytes());
        packet.push(destination.len() as u8);
        packet.extend_from_slice(destination);
        packet.push(source.len() as u8);
        packet.extend_from_slice(source);
        if (version == 1 && kind == 0) || (version == 0x6b33_43cf && kind == 1) {
            packet.push(0);
        }
        if version != 0 && !((version == 1 && kind == 3) || (version == 0x6b33_43cf && kind == 0)) {
            packet.push(20);
        }
        packet.extend_from_slice(&[0; 20]);
        packet
    }

    pub fn short(destination: &[u8]) -> Vec<u8> {
        let mut packet = vec![0x40];
        packet.extend_from_slice(destination);
        packet.extend_from_slice(&[0; 20]);
        packet
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{long, short};
    use super::*;

    #[test]
    fn versions_and_coalesced_packets_are_parsed_without_unprotecting_headers() {
        for (version, initial, handshake) in [(VERSION_1, 0, 2), (VERSION_2, 1, 3)] {
            let mut datagram = long(version, initial, b"server", b"client");
            datagram.extend_from_slice(&long(version, handshake, b"server", b"client"));
            datagram.extend_from_slice(&short(b"server"));
            let packets: Vec<_> = Packets(&datagram).collect();
            assert_eq!(
                packets.iter().map(|packet| packet.kind).collect::<Vec<_>>(),
                [Kind::Initial, Kind::Handshake, Kind::OneRtt]
            );
            assert_eq!(packets[0].source, b"client");
            assert_eq!(packets[1].destination, b"server");
        }
    }

    #[test]
    fn real_sized_initials_and_all_varint_widths_are_supported() {
        let mut initial = vec![0xc0, 0, 0, 0, 1, 8];
        initial.extend_from_slice(&[1; 8]);
        initial.extend_from_slice(&[0, 0, 0x44, 0x9e]);
        initial.resize(1200, 0);
        assert_eq!(Connection::default().uplink(&initial).attempts, 1);
        for width in [1, 2, 4, 8] {
            let mut encoded = vec![0; width];
            encoded[0] = match width {
                1 => 0,
                2 => 0x40,
                4 => 0x80,
                _ => 0xc0,
            };
            encoded[width - 1] |= 20;
            let mut bytes = encoded.as_slice();
            assert_eq!(varint(&mut bytes), Some(20));
            assert!(bytes.is_empty());
            for truncated in 0..width {
                assert!(varint(&mut &encoded[..truncated]).is_none());
            }
        }
    }

    #[test]
    fn one_rtt_is_last_and_greased_bits_do_not_change_cid_matching() {
        let mut connection = Connection::default();
        connection.uplink(&long(VERSION_1, 0, b"original", b"client"));
        connection.uplink(&long(VERSION_1, 2, b"server", b"client"));
        let mut datagram = short(b"server");
        datagram[0] = 0x1f;
        datagram.extend_from_slice(&long(VERSION_1, 0, b"other", b"spurious"));
        assert_eq!(connection.uplink(&datagram).attempts, 0);
        assert_eq!(connection.downlink(&short(b"client")).exchanges, 1);
    }

    #[test]
    fn retransmissions_retry_and_compatible_version_changes_are_one_attempt() {
        let mut connection = Connection::default();
        let initial = long(VERSION_1, 0, b"original", b"client");
        assert_eq!(connection.uplink(&initial).attempts, 1);
        assert_eq!(connection.uplink(&initial).attempts, 0);
        connection.downlink(&long(VERSION_1, 3, b"client", b"retry"));
        assert_eq!(
            connection
                .uplink(&long(VERSION_1, 0, b"retry", b"client"))
                .attempts,
            0
        );
        assert_eq!(
            connection
                .uplink(&long(VERSION_2, 1, b"retry", b"client"))
                .attempts,
            0
        );
        assert_eq!(connection.attempts.len(), 1);
    }

    #[test]
    fn cid_rotation_is_not_a_reconnect_and_exchanges_are_reported_once() {
        let mut connection = Connection::default();
        connection.uplink(&long(VERSION_1, 0, b"original", b"client"));
        connection.downlink(&long(VERSION_1, 0, b"client", b"server"));
        connection.uplink(&long(VERSION_1, 2, b"server", b"client"));
        assert_eq!(connection.uplink(&short(b"server")).exchanges, 0);
        assert_eq!(connection.downlink(&short(b"client")).exchanges, 1);
        assert_eq!(connection.downlink(&short(b"client")).exchanges, 0);
        assert_eq!(connection.uplink(&short(b"rotated")).attempts, 0);
        assert_eq!(connection.downlink(&short(b"rotated")).exchanges, 0);
        assert_eq!(connection.attempts.len(), 1);
    }

    #[test]
    fn a_new_initial_is_detected_even_when_the_routing_prefix_is_retained() {
        let mut connection = Connection::default();
        connection.uplink(&long(VERSION_1, 0, b"same-prefix-old", b"client"));
        connection.uplink(&long(VERSION_1, 2, b"same-prefix-old", b"client"));
        assert_eq!(
            connection
                .uplink(&long(VERSION_1, 0, b"same-prefix-new", b"new-client"))
                .attempts,
            1
        );
        assert_eq!(
            connection
                .uplink(&long(VERSION_1, 0, b"same-prefix-old", b"client"))
                .attempts,
            1
        );
    }

    #[test]
    fn a_failed_parallel_candidate_does_not_erase_the_working_attempt() {
        let mut connection = Connection::default();
        connection.uplink(&long(VERSION_1, 0, b"original", b"working"));
        connection.uplink(&long(VERSION_1, 2, b"server", b"working"));
        connection.uplink(&long(VERSION_1, 0, b"other", b"candidate"));
        connection.uplink(&short(b"server"));
        assert_eq!(connection.downlink(&short(b"working")).exchanges, 1);
        assert_eq!(connection.attempts.len(), 2);
    }

    #[test]
    fn zero_length_client_ids_can_follow_retry() {
        let mut connection = Connection::default();
        assert_eq!(
            connection
                .uplink(&long(VERSION_1, 0, b"original", b""))
                .attempts,
            1
        );
        connection.downlink(&long(VERSION_1, 3, b"", b"retry"));
        assert_eq!(
            connection
                .uplink(&long(VERSION_1, 0, b"retry", b""))
                .attempts,
            0
        );
    }

    #[test]
    fn malformed_truncated_and_unknown_packets_do_not_panic_or_emit_attempts() {
        let initial = long(VERSION_1, 0, b"server", b"client");
        for length in 0..initial.len() {
            assert_eq!(Connection::default().uplink(&initial[..length]).attempts, 0);
        }
        let mut invalid = initial.clone();
        invalid[5] = 21;
        assert_eq!(Connection::default().uplink(&invalid).attempts, 0);
        assert_eq!(
            Connection::default()
                .uplink(&long(1234, 0, b"server", b"client"))
                .attempts,
            0
        );
        let mut invalid_length = initial;
        invalid_length[20] = 63;
        assert_eq!(Connection::default().uplink(&invalid_length).attempts, 0);
        for first in 0..=255 {
            for length in 0..64 {
                let mut bytes = vec![0xff; length];
                if let Some(byte) = bytes.first_mut() {
                    *byte = first;
                }
                let mut connection = Connection::default();
                connection.uplink(&bytes);
                connection.downlink(&bytes);
            }
        }
    }

    #[test]
    fn candidate_storage_is_bounded() {
        let mut connection = Connection::default();
        for index in 0..32 {
            connection.uplink(&long(VERSION_1, 0, b"server", &[index]));
        }
        assert_eq!(connection.attempts.len(), MAX_ATTEMPTS);
    }
}
