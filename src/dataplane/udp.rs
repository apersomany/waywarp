// Carries the private namespace's UDP out of the physical interface, one host socket per flow so
// each reply maps back to exactly one private source. While bootstrapping, tunnel flows use a
// SOCKS5 relay and other underlay UDP stays direct; `migrate` moves the tunnel flows to direct sockets.
use super::packet::{self, HEADERS};
use crate::via::interface::Interface;
use crate::via::{Relay, Route, socks5};
use mio::net::UdpSocket;
use mio::unix::SourceFd;
use mio::{Interest, Registry, Token, Waker};
use slab::Slab;
use std::collections::HashMap;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, SocketAddrV4, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

pub const MAX_FLOWS: usize = 1024;
const MAX_PENDING: usize = 64;
const MAX_QUEUED_PACKETS: usize = 32;
const MAX_QUEUED_BYTES: usize = 64 * 1024;
const IDLE: Duration = Duration::from_secs(180);
// Received payloads start here so IPv4/UDP headers can be written in front without copying.
const PAYLOAD: usize = HEADERS;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    source: SocketAddrV4,
    destination: SocketAddrV4,
}

pub(super) struct Association {
    socket: UdpSocket,
    relay: SocketAddrV4,
    // The relay drops the association when this TCP control connection closes.
    _control: TcpStream,
}

pub(super) fn prepare(
    interface: &Interface,
    relay: &crate::via::Relay,
) -> anyhow::Result<Association> {
    let (control, address) = socks5::associate(interface, relay)?;
    Ok(Association {
        socket: UdpSocket::from_std(interface.udp(address)?),
        relay: address,
        _control: control,
    })
}

pub(super) enum Routing {
    Blocked,
    Direct,
    Relayed {
        relay: Relay,
        edge: SocketAddrV4,
        prepared: Option<Association>,
    },
}

impl Routing {
    pub fn prepare(
        interface: &Interface,
        route: Route,
        edge: SocketAddrV4,
    ) -> anyhow::Result<Self> {
        Ok(match route {
            Route::Direct => Self::Direct,
            Route::Relay(relay) => {
                let association = prepare(interface, &relay)?;
                Self::Relayed {
                    relay,
                    edge,
                    prepared: Some(association),
                }
            }
        })
    }
}

// Preserve Initial packets while authentication is paced; keeping only the latest datagram
// can strand QUIC's handshake. Drop new arrivals when full, never overwrite the oldest ones.
#[derive(Default)]
struct Pending {
    packets: Vec<Vec<u8>>,
    bytes: usize,
}

impl Pending {
    fn push(&mut self, payload: &[u8]) {
        if self.packets.len() < MAX_QUEUED_PACKETS && self.bytes + payload.len() <= MAX_QUEUED_BYTES
        {
            self.packets.push(payload.to_vec());
            self.bytes += payload.len();
        }
    }
}

enum Path {
    Direct,
    Pending(Pending),
    Relayed(Association),
}

struct Flow {
    key: Key,
    generation: u64,
    direct: UdpSocket,
    path: Path,
    last_used: Instant,
}

type Associated = (usize, u64, anyhow::Result<Association>);

pub struct Flows {
    tun: File,
    interface: Interface,
    routing: Routing,
    slab: Slab<Flow>,
    keys: HashMap<Key, usize>,
    base: usize,
    generation: u64,
    pending: usize,
    buffer: Box<[u8]>,
    results: (mpsc::Sender<Associated>, mpsc::Receiver<Associated>),
    waker: Arc<Waker>,
    last_sweep: Instant,
    // Answered once an edge replies on a direct socket after `migrate`.
    migrated: Option<(SocketAddrV4, mpsc::Sender<()>)>,
}

impl Flows {
    pub fn new(tun: File, interface: Interface, waker: Arc<Waker>, base: usize) -> Self {
        Self {
            tun,
            interface,
            routing: Routing::Blocked,
            slab: Slab::new(),
            keys: HashMap::new(),
            base,
            generation: 0,
            pending: 0,
            buffer: vec![0; 65536].into_boxed_slice(),
            results: mpsc::channel(),
            waker,
            last_sweep: Instant::now(),
            migrated: None,
        }
    }

    pub fn register(&self, registry: &Registry, token: Token) -> std::io::Result<()> {
        registry.register(
            &mut SourceFd(&self.tun.as_raw_fd()),
            token,
            Interest::READABLE,
        )
    }

    fn token(&self, index: usize, relayed: bool) -> Token {
        Token(self.base + index * 2 + usize::from(relayed))
    }

    pub fn owns(&self, token: Token) -> bool {
        (self.base..self.base + MAX_FLOWS * 2).contains(&token.0)
    }

    fn close(&mut self, registry: &Registry, index: usize) {
        if let Some(mut flow) = self.slab.try_remove(index) {
            let _ = registry.deregister(&mut flow.direct);
            if let Path::Relayed(association) = &mut flow.path {
                let _ = registry.deregister(&mut association.socket);
            }
            self.keys.remove(&flow.key);
        }
    }

    fn insert(
        &mut self,
        registry: &Registry,
        key: Key,
        mut direct: UdpSocket,
        pending: bool,
    ) -> Option<(usize, u64)> {
        if self.slab.len() >= MAX_FLOWS {
            let oldest = self
                .slab
                .iter()
                .min_by_key(|(_, flow)| flow.last_used)
                .map(|(index, _)| index)?;
            self.close(registry, oldest);
        }
        let index = self.slab.vacant_key();
        if let Err(error) =
            registry.register(&mut direct, self.token(index, false), Interest::READABLE)
        {
            warn!(%error, "cannot register a UDP flow");
            return None;
        }
        self.generation += 1;
        self.slab.insert(Flow {
            key,
            generation: self.generation,
            direct,
            path: if pending {
                Path::Pending(Pending::default())
            } else {
                Path::Direct
            },
            last_used: Instant::now(),
        });
        self.keys.insert(key, index);
        Some((index, self.generation))
    }

    pub fn tun_ready(&mut self, registry: &Registry) {
        loop {
            let length = match self.tun.read(&mut self.buffer) {
                Ok(length) => length,
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => {
                    warn!(%error, "TUN read failed");
                    break;
                }
            };
            let Some(datagram) = packet::outbound(&self.buffer[..length]) else {
                continue;
            };
            let key = Key {
                source: datagram.source,
                destination: datagram.destination,
            };
            let Some(index) = self
                .keys
                .get(&key)
                .copied()
                .or_else(|| self.open(registry, key))
            else {
                continue;
            };
            let flow = &mut self.slab[index];
            flow.last_used = Instant::now();
            let payload = &self.buffer[datagram.payload];
            match &mut flow.path {
                Path::Direct => send(&flow.direct, payload, key.destination),
                Path::Pending(queued) => queued.push(payload),
                Path::Relayed(association) => relay_send(association, key.destination, payload),
            }
        }
    }

    fn open(&mut self, registry: &Registry, key: Key) -> Option<usize> {
        let relay = match &self.routing {
            Routing::Blocked => return None,
            Routing::Relayed {
                relay,
                edge,
                prepared,
            } if *edge == key.destination => {
                if prepared.is_none() && self.pending >= MAX_PENDING {
                    return None;
                }
                Some(relay.clone())
            }
            _ => None,
        };
        // Resolved per flow, so a new network takes over as soon as WARP reconnects.
        let direct = match self.interface.udp(key.destination) {
            Ok(socket) => UdpSocket::from_std(socket),
            Err(error) => {
                warn!(destination = %key.destination, error = format!("{error:#}"), "cannot open a UDP socket");
                return None;
            }
        };
        let (index, generation) = self.insert(registry, key, direct, relay.is_some())?;
        if let Some(relay) = relay {
            if let Routing::Relayed { prepared, .. } = &mut self.routing
                && let Some(mut association) = prepared.take()
            {
                if let Err(error) = registry.register(
                    &mut association.socket,
                    self.token(index, true),
                    Interest::READABLE,
                ) {
                    warn!(%error, "cannot register a prepared UDP association");
                    self.close(registry, index);
                    return None;
                }
                self.slab[index].path = Path::Relayed(association);
                return Some(index);
            }
            self.pending += 1;
            let interface = self.interface.clone();
            let results = self.results.0.clone();
            let waker = Arc::clone(&self.waker);
            std::thread::spawn(move || {
                let result = prepare(&interface, &relay);
                let _ = results.send((index, generation, result));
                let _ = waker.wake();
            });
        }
        Some(index)
    }

    pub fn associations_ready(&mut self, registry: &Registry) {
        while let Ok((index, generation, result)) = self.results.1.try_recv() {
            self.pending -= 1;
            let token = self.token(index, true);
            let Some(flow) = self
                .slab
                .get_mut(index)
                .filter(|flow| flow.generation == generation)
            else {
                continue;
            };
            let Path::Pending(queued) = &mut flow.path else {
                continue;
            };
            let queued = std::mem::take(queued);
            let key = flow.key;
            let association = result.and_then(|mut association| {
                registry.register(&mut association.socket, token, Interest::READABLE)?;
                Ok(association)
            });
            match association {
                Ok(association) => {
                    for payload in queued.packets {
                        relay_send(&association, key.destination, &payload);
                    }
                    flow.path = Path::Relayed(association);
                }
                Err(error) => {
                    debug!(
                        destination = %key.destination,
                        error = format!("{error:#}"),
                        "SOCKS5 association failed"
                    );
                    self.close(registry, index);
                }
            }
        }
    }

    pub fn reset(&mut self, registry: &Registry, routing: Routing) {
        let indices: Vec<_> = self.slab.iter().map(|(index, _)| index).collect();
        for index in indices {
            self.close(registry, index);
        }
        self.routing = routing;
        self.migrated = None;
    }

    // Abandons the relay: every flow continues on its direct socket, whose new source address
    // QUIC connection migration accepts, so the edge session and its colo carry over.
    pub fn migrate(&mut self, registry: &Registry, answered: mpsc::Sender<()>) {
        self.migrated = match std::mem::replace(&mut self.routing, Routing::Direct) {
            Routing::Relayed { edge, .. } => Some((edge, answered)),
            _ => None,
        };
        for (_, flow) in self.slab.iter_mut() {
            if let Path::Relayed(association) = &mut flow.path {
                let _ = registry.deregister(&mut association.socket);
            }
            flow.path = Path::Direct;
        }
    }

    pub fn socket_ready(&mut self, token: Token) {
        let offset = token.0 - self.base;
        let (index, relayed) = (offset / 2, offset % 2 == 1);
        let Some(flow) = self.slab.get_mut(index) else {
            return;
        };
        let key = flow.key;
        loop {
            let (socket, start, expected) = match (&flow.path, relayed) {
                (Path::Direct, false) => (&flow.direct, PAYLOAD, SocketAddr::V4(key.destination)),
                (Path::Relayed(association), true) => (
                    &association.socket,
                    PAYLOAD - socks5::HEADER,
                    SocketAddr::V4(association.relay),
                ),
                // Late replies on an abandoned path are dropped by leaving them unread.
                _ => return,
            };
            let (length, sender) = match socket.recv_from(&mut self.buffer[start..]) {
                Ok(received) => received,
                Err(error) if error.kind() == ErrorKind::WouldBlock => return,
                Err(error) if error.kind() == ErrorKind::ConnectionRefused => continue,
                Err(error) => {
                    debug!(destination = %key.destination, %error, "UDP flow failed");
                    return;
                }
            };
            if sender != expected {
                continue;
            }
            let payload = if relayed {
                if socks5::payload(key.destination, &self.buffer[start..start + length]).is_none() {
                    continue;
                }
                length - socks5::HEADER
            } else {
                if let Some((_, answered)) =
                    self.migrated.take_if(|(edge, _)| *edge == key.destination)
                {
                    let _ = answered.send(());
                }
                length
            };
            if payload > packet::MAX_PAYLOAD {
                continue;
            }
            flow.last_used = Instant::now();
            let frame = packet::inbound(&mut self.buffer, payload, key.destination, key.source);
            if let Err(error) = (&self.tun).write(frame)
                && error.kind() != ErrorKind::WouldBlock
            {
                warn!(%error, "TUN write failed");
            }
        }
    }

    pub fn sweep(&mut self, registry: &Registry) {
        let now = Instant::now();
        if now.duration_since(self.last_sweep) < Duration::from_secs(15) {
            return;
        }
        self.last_sweep = now;
        let idle: Vec<_> = self
            .slab
            .iter()
            .filter(|(_, flow)| now.duration_since(flow.last_used) >= IDLE)
            .map(|(index, _)| index)
            .collect();
        for index in idle {
            self.close(registry, index);
        }
    }
}

fn send(socket: &UdpSocket, payload: &[u8], destination: SocketAddrV4) {
    if let Err(error) = socket.send_to(payload, destination.into())
        && !matches!(
            error.kind(),
            ErrorKind::WouldBlock | ErrorKind::ConnectionRefused
        )
    {
        debug!(%destination, %error, "UDP send failed");
    }
}

fn relay_send(association: &Association, destination: SocketAddrV4, payload: &[u8]) {
    let mut frame = Vec::with_capacity(socks5::HEADER + payload.len());
    frame.extend_from_slice(&socks5::header(destination));
    frame.extend_from_slice(payload);
    send(&association.socket, &frame, association.relay);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dns_stays_direct_and_cannot_confirm_tunnel_migration() {
        let poll = mio::Poll::new().unwrap();
        let waker = Arc::new(Waker::new(poll.registry(), Token(0)).unwrap());
        let tun = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap();
        let mut flows = Flows::new(tun, Interface::Named("lo".into()), waker, 16);
        let tunnel = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let resolver = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let address = |socket: &std::net::UdpSocket| {
            SocketAddrV4::new(
                std::net::Ipv4Addr::LOCALHOST,
                socket.local_addr().unwrap().port(),
            )
        };
        let edge = address(&tunnel);
        let source = "10.79.0.2:5000".parse().unwrap();
        let dns = Key {
            source,
            destination: address(&resolver),
        };
        assert!(flows.open(poll.registry(), dns).is_none());
        flows.routing = Routing::Relayed {
            relay: Relay {
                label: "test".into(),
                server: edge,
                login: None,
            },
            edge,
            prepared: None,
        };
        flows.pending = MAX_PENDING;
        let dns_index = flows.open(poll.registry(), dns).unwrap();
        assert!(matches!(flows.slab[dns_index].path, Path::Direct));
        assert_eq!(flows.pending, MAX_PENDING);
        let tunnel_key = Key {
            source,
            destination: edge,
        };
        assert!(flows.open(poll.registry(), tunnel_key).is_none());
        let (answered, answer) = mpsc::channel();
        flows.migrate(poll.registry(), answered);
        let reply = |socket: &std::net::UdpSocket, flow: &Flow| {
            socket
                .send_to(
                    b"reply",
                    ("127.0.0.1", flow.direct.local_addr().unwrap().port()),
                )
                .unwrap();
        };
        reply(&resolver, &flows.slab[dns_index]);
        flows.socket_ready(flows.token(dns_index, false));
        assert!(matches!(answer.try_recv(), Err(mpsc::TryRecvError::Empty)));
        let tunnel_index = flows.open(poll.registry(), tunnel_key).unwrap();
        reply(&tunnel, &flows.slab[tunnel_index]);
        flows.socket_ready(flows.token(tunnel_index, false));
        assert_eq!(answer.try_recv(), Ok(()));
    }

    #[test]
    fn pending_packets_preserve_order_and_initial_packet() {
        let mut pending = Pending::default();
        for index in 0..MAX_QUEUED_PACKETS + 5 {
            pending.push(&[index as u8]);
        }
        assert_eq!(pending.packets.len(), MAX_QUEUED_PACKETS);
        assert_eq!(pending.bytes, MAX_QUEUED_PACKETS);
        for (index, packet) in pending.packets.iter().enumerate() {
            assert_eq!(packet, &[index as u8]);
        }
    }

    #[test]
    fn pending_packets_have_a_byte_limit() {
        let mut pending = Pending::default();
        pending.push(&vec![1; MAX_QUEUED_BYTES]);
        pending.push(&[2]);
        assert_eq!(pending.packets.len(), 1);
        assert_eq!(pending.bytes, MAX_QUEUED_BYTES);
    }
}
