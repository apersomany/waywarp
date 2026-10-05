// The physical interface that carries every packet leaving an instance. Sockets bound to it
// bypass the private namespace's routes and any host VPN.
use anyhow::{Context, Result, bail};
use nix::{ifaddrs::getifaddrs, libc};
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Interface {
    // Pinned with --interface.
    Named(String),
    // Follows the host's routes, so moving between networks moves new sockets with it.
    Routed,
}

fn connect_socket(socket: Socket, destination: SocketAddr) -> Result<TcpStream> {
    socket.set_nonblocking(true)?;
    match socket.connect(&destination.into()) {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == Some(libc::EINPROGRESS) => {}
        Err(error) => return Err(error.into()),
    }
    Ok(socket.into())
}

// Uses the calling namespace's routes, without stalling the packet loop during establishment.
pub fn connect_tcp(destination: SocketAddr) -> Result<TcpStream> {
    connect_socket(
        Socket::new(
            Domain::for_address(destination),
            Type::STREAM.cloexec(),
            Some(Protocol::TCP),
        )?,
        destination,
    )
}

fn physical(name: &str) -> bool {
    let link = Path::new("/sys/class/net").join(name);
    link.join("device").exists() && !link.join("tun_flags").exists()
}

// Names the physical interface that host routing, including policy rules, picks for `destination`.
fn route(destination: SocketAddrV4) -> Result<String> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket
        .connect(&SocketAddr::V4(destination).into())
        .with_context(|| format!("no route to {}", destination.ip()))?;
    let source = socket
        .local_addr()?
        .as_socket()
        .context("no local route")?
        .ip();
    let ipv4 = |address: &nix::ifaddrs::InterfaceAddress| {
        address
            .address
            .as_ref()
            .and_then(|value| value.as_sockaddr_in())
            .map(|value| IpAddr::V4(value.ip()))
    };
    let routed: BTreeSet<_> = getifaddrs()?
        .filter(|address| ipv4(address) == Some(source))
        .map(|address| address.interface_name)
        .collect();
    if let Some(name) = routed.into_iter().find(|name| physical(name)) {
        return Ok(name);
    }
    let candidates: BTreeSet<_> = getifaddrs()?
        .filter(|address| ipv4(address).is_some())
        .map(|address| address.interface_name)
        .filter(|name| physical(name))
        .collect();
    let list = candidates.into_iter().collect::<Vec<_>>().join(", ");
    bail!(
        "the route to {} leaves through a virtual link; choose a physical interface with --interface ({})",
        destination.ip(),
        if list.is_empty() { "none found" } else { &list }
    )
}

impl Interface {
    pub fn new(explicit: Option<String>) -> Result<Self> {
        match explicit {
            Some(name) if !Path::new("/sys/class/net").join(&name).exists() => {
                bail!("interface {name} does not exist")
            }
            Some(name) => Ok(Self::Named(name)),
            None => Ok(Self::Routed),
        }
    }

    pub fn name(&self, destination: SocketAddrV4) -> Result<String> {
        match self {
            Self::Named(name) => Ok(name.clone()),
            Self::Routed => route(destination),
        }
    }

    fn socket(&self, destination: SocketAddrV4, kind: Type) -> Result<Socket> {
        let name = self.name(destination)?;
        let protocol = if kind == Type::DGRAM {
            Protocol::UDP
        } else {
            Protocol::TCP
        };
        let socket = Socket::new(Domain::IPV4, kind.cloexec(), Some(protocol))?;
        socket
            .bind_device(Some(name.as_bytes()))
            .with_context(|| format!("binding a socket to {name}"))?;
        Ok(socket)
    }

    // A nonblocking UDP socket for sending to `destination`.
    pub fn udp(&self, destination: SocketAddrV4) -> Result<UdpSocket> {
        let socket = self.socket(destination, Type::DGRAM)?;
        socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)).into())?;
        socket.set_nonblocking(true)?;
        Ok(socket.into())
    }

    // A nonblocking TCP connection to `destination` that is still being established.
    pub fn tcp(&self, destination: SocketAddrV4) -> Result<TcpStream> {
        connect_socket(self.socket(destination, Type::STREAM)?, destination.into())
    }
}
