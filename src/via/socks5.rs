use super::Relay;
use super::interface::Interface;
use super::transport::Stream;
use crate::dataplane::Observer;
use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};

// Connects to the relay and completes SOCKS5 method negotiation and authentication.
fn handshake(interface: &Interface, relay: &Relay, observer: &Observer) -> Result<Stream> {
    let authenticate = || handshake_unpaced(interface, relay, observer);
    match relay
        .login
        .as_ref()
        .and_then(|login| login.auth_limit.as_deref())
    {
        Some(path) => super::auth_limit::run(path, observer, authenticate),
        None => authenticate(),
    }
}

fn handshake_unpaced(interface: &Interface, relay: &Relay, observer: &Observer) -> Result<Stream> {
    let mut control = Stream::connect(interface, relay.server, observer)
        .with_context(|| format!("connecting to SOCKS5 relay {}", relay.label))?;
    let method = if relay.login.is_some() { 2 } else { 0 };
    control.write_all(&[5, 1, method])?;
    let mut reply = [0; 2];
    control.read_exact(&mut reply)?;
    if reply != [5, method] {
        bail!("SOCKS5 relay rejected the authentication method");
    }
    if let Some(login) = &relay.login {
        let mut packet = vec![1, login.username.len() as u8];
        packet.extend_from_slice(login.username.as_bytes());
        packet.push(login.password.len() as u8);
        packet.extend_from_slice(login.password.as_bytes());
        control.write_all(&packet)?;
        control.read_exact(&mut reply)?;
        if reply != [1, 0] {
            bail!("SOCKS5 relay rejected the credentials");
        }
    }
    Ok(control)
}

// Reads a SOCKS5 reply and returns the bound address it carries.
fn reply(control: &mut Stream, relay: &Relay, refused: &str) -> Result<SocketAddrV4> {
    let mut header = [0; 4];
    control.read_exact(&mut header)?;
    if header[..3] != [5, 0, 0] {
        bail!("SOCKS5 relay refused {refused} (reply code {})", header[1]);
    }
    let address = match header[3] {
        1 => {
            let mut bound = [0; 6];
            control.read_exact(&mut bound)?;
            SocketAddrV4::new(
                Ipv4Addr::new(bound[0], bound[1], bound[2], bound[3]),
                u16::from_be_bytes([bound[4], bound[5]]),
            )
        }
        3 | 4 => {
            let length = if header[3] == 4 {
                16
            } else {
                let mut length = [0; 1];
                control.read_exact(&mut length)?;
                usize::from(length[0])
            };
            let mut skipped = vec![0; length + 2];
            control.read_exact(&mut skipped)?;
            SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)
        }
        other => bail!("SOCKS5 relay sent address type {other}"),
    };
    Ok(if address.ip().is_unspecified() {
        SocketAddrV4::new(*relay.server.ip(), address.port())
    } else {
        address
    })
}

// Performs a SOCKS5 UDP ASSOCIATE; the association lives as long as the returned stream.
pub fn associate(
    interface: &Interface,
    relay: &Relay,
    observer: &Observer,
) -> Result<(TcpStream, SocketAddrV4)> {
    let mut control = handshake(interface, relay, observer)?;
    control.write_all(&[5, 3, 0, 1, 0, 0, 0, 0, 0, 0])?;
    let address = reply(&mut control, relay, "UDP association")?;
    if address.port() == 0 {
        bail!("SOCKS5 relay returned port zero for UDP");
    }
    Ok((control.into_inner(), address))
}

// Opens a TCP stream to `host` through the relay, which resolves the name itself.
pub fn connect(
    interface: &Interface,
    relay: &Relay,
    host: &str,
    port: u16,
    observer: &Observer,
) -> Result<Stream> {
    let mut control = handshake(interface, relay, observer)?;
    let mut request = vec![5, 1, 0, 3, host.len() as u8];
    request.extend_from_slice(host.as_bytes());
    request.extend_from_slice(&port.to_be_bytes());
    control.write_all(&request)?;
    reply(&mut control, relay, "the TCP connection")?;
    Ok(control)
}

// Length of the UDP request header for an IPv4 destination (RFC 1928 section 7).
pub const HEADER: usize = 10;

pub fn header(destination: SocketAddrV4) -> [u8; HEADER] {
    let mut header = [0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    header[4..8].copy_from_slice(&destination.ip().octets());
    header[8..].copy_from_slice(&destination.port().to_be_bytes());
    header
}

// Strips the header from a relayed datagram, rejecting fragments and other sources.
pub fn payload(source: SocketAddrV4, frame: &[u8]) -> Option<&[u8]> {
    (frame.len() >= HEADER && frame[..HEADER] == header(source)).then(|| &frame[HEADER..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::via::Login;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    fn relay(listener: &TcpListener, authenticated: bool) -> Relay {
        let std::net::SocketAddr::V4(server) = listener.local_addr().unwrap() else {
            unreachable!()
        };
        Relay {
            label: "test".into(),
            server,
            login: authenticated.then(|| Login {
                username: "u".into(),
                password: "p".into(),
                auth_limit: None,
            }),
        }
    }

    #[test]
    fn associations_preserve_authentication_addresses_and_control_lifetime() {
        for authenticated in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let relay = relay(&listener, authenticated);
            let server = thread::spawn(move || {
                let (mut control, _) = listener.accept().unwrap();
                control
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let method = if authenticated { 2 } else { 0 };
                let mut greeting = [0; 3];
                control.read_exact(&mut greeting).unwrap();
                assert_eq!(greeting, [5, 1, method]);
                control.write_all(&[5, method]).unwrap();
                if authenticated {
                    let mut credentials = [0; 5];
                    control.read_exact(&mut credentials).unwrap();
                    assert_eq!(credentials, [1, 1, b'u', 1, b'p']);
                    control.write_all(&[1, 0]).unwrap();
                }
                let mut request = [0; 10];
                control.read_exact(&mut request).unwrap();
                assert_eq!(request, [5, 3, 0, 1, 0, 0, 0, 0, 0, 0]);
                control
                    .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 4, 210])
                    .unwrap();
                assert_eq!(control.read(&mut [0]).unwrap(), 0);
            });
            let (control, address) =
                associate(&Interface::Named("lo".into()), &relay, &Observer::default()).unwrap();
            assert_eq!(address, SocketAddrV4::new(*relay.server.ip(), 1234));
            drop(control);
            server.join().unwrap();
        }
    }

    #[test]
    fn cancellation_interrupts_every_socks5_reply_stage() {
        for stage in 0..4 {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let relay = relay(&listener, true);
            let observer = Observer::default();
            let cancelled = observer.clone();
            let (finished, result) = mpsc::channel();
            let worker = thread::spawn(move || {
                finished
                    .send(associate(
                        &Interface::Named("lo".into()),
                        &relay,
                        &cancelled,
                    ))
                    .unwrap();
            });
            let (mut control, _) = listener.accept().unwrap();
            control
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut greeting = [0; 3];
            control.read_exact(&mut greeting).unwrap();
            assert_eq!(greeting, [5, 1, 2]);
            if stage > 0 {
                control.write_all(&[5, 2]).unwrap();
                let mut credentials = [0; 5];
                control.read_exact(&mut credentials).unwrap();
                assert_eq!(credentials, [1, 1, b'u', 1, b'p']);
            }
            if stage > 1 {
                control.write_all(&[1, 0]).unwrap();
                let mut request = [0; 10];
                control.read_exact(&mut request).unwrap();
                assert_eq!(request, [5, 3, 0, 1, 0, 0, 0, 0, 0, 0]);
            }
            if stage > 2 {
                control.write_all(&[5, 0, 0, 1]).unwrap();
            }
            observer.stop();
            let completed = result.recv_timeout(Duration::from_secs(1));
            drop(control);
            worker.join().unwrap();
            assert_eq!(
                completed.unwrap().unwrap_err().root_cause().to_string(),
                "relay operation was cancelled",
                "reply stage {stage}"
            );
        }
    }

    #[test]
    fn socks5_frames_round_trip_and_reject_fragments_or_other_sources() {
        let edge = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 7), 443);
        let mut frame = header(edge).to_vec();
        frame.extend_from_slice(b"payload");
        assert_eq!(payload(edge, &frame), Some(&b"payload"[..]));
        let other = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 8), 443);
        assert_eq!(payload(other, &frame), None);
        frame[2] = 1;
        assert_eq!(payload(edge, &frame), None);
    }
}
