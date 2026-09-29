// Screens relays before WARP uses one: a QUIC round trip through the relay proves UDP reaches the
// edge, and a trace through it hints at the colo that would serve it.
use super::interface::Interface;
use super::{Relay, socks5};
use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::net::SocketAddrV4;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

const CONCURRENCY: usize = 4;
const TIMEOUT: Duration = Duration::from_secs(4);
// Resolved by the relay itself; a Cloudflare anycast name lands on the colo near the relay.
const TRACE_HOST: &str = "www.cloudflare.com";

pub struct Ping {
    pub rtt: Duration,
    pub colo: Option<String>,
}

// A QUIC long header with a reserved version makes any QUIC server answer with Version
// Negotiation (RFC 9000 section 6), which proves UDP reaches the edge through the relay.
fn version_negotiation(connection: &[u8; 8]) -> Vec<u8> {
    let mut packet = vec![0xc0, 0x1a, 0x2a, 0x3a, 0x4a, 8];
    packet.extend_from_slice(connection);
    packet.extend_from_slice(&[8]);
    packet.extend_from_slice(connection);
    packet.resize(1200, 0);
    packet
}

fn is_version_negotiation(packet: &[u8], connection: &[u8; 8]) -> bool {
    packet.len() >= 23
        && packet[0] & 0x80 != 0
        && packet[1..5] == [0; 4]
        && packet[6..14] == connection[..]
}

fn udp_rtt(interface: &Interface, relay: &Relay, edge: SocketAddrV4) -> Result<Duration> {
    let (_control, association) = socks5::associate(interface, relay)?;
    let socket = interface.udp(association)?;
    socket.set_nonblocking(false)?;
    socket.connect(association)?;
    let connection = super::seed().to_be_bytes();
    let mut frame = socks5::header(edge).to_vec();
    frame.extend_from_slice(&version_negotiation(&connection));
    let deadline = Instant::now() + TIMEOUT;
    let mut buffer = [0; 2048];
    let started = Instant::now();
    socket.send(&frame)?;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("the edge did not answer over UDP");
        }
        socket.set_read_timeout(Some(remaining))?;
        let length = socket
            .recv(&mut buffer)
            .context("the edge did not answer over UDP")?;
        if let Some(payload) = socks5::payload(edge, &buffer[..length])
            && is_version_negotiation(payload, &connection)
        {
            return Ok(started.elapsed());
        }
    }
}

fn trace_colo(interface: &Interface, relay: &Relay) -> Result<String> {
    let mut stream = socks5::connect(interface, relay, TRACE_HOST, 80)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    write!(
        stream,
        "GET /cdn-cgi/trace HTTP/1.1\r\nHost: {TRACE_HOST}\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = Vec::new();
    stream.take(16 * 1024).read_to_end(&mut response)?;
    let text = String::from_utf8_lossy(&response);
    text.lines()
        .find_map(|line| line.strip_prefix("colo="))
        .map(|colo| colo.trim().to_ascii_uppercase())
        .context("the trace response had no colo")
}

pub fn ping(interface: &Interface, relay: &Relay, edge: SocketAddrV4) -> Result<Ping> {
    let rtt = udp_rtt(interface, relay, edge)?;
    // The colo is only a hint: WARP reaches a different anycast address over UDP.
    let colo = trace_colo(interface, relay).ok();
    Ok(Ping { rtt, colo })
}

// Pings up to CONCURRENCY relays at a time and yields results as they finish, so connecting can
// start as soon as a good relay is known while the rest are still being pinged.
pub fn stream(
    interface: &Interface,
    relays: Vec<Relay>,
    edge: SocketAddrV4,
) -> mpsc::Receiver<(Relay, Result<Ping>)> {
    let queue = Arc::new(Mutex::new(relays.into_iter()));
    let (sender, receiver) = mpsc::channel();
    for _ in 0..CONCURRENCY {
        let (queue, sender, interface) = (Arc::clone(&queue), sender.clone(), interface.clone());
        thread::spawn(move || {
            loop {
                let Some(relay) = queue.lock().unwrap().next() else {
                    return;
                };
                let ping = ping(&interface, &relay, edge);
                if sender.send((relay, ping)).is_err() {
                    return;
                }
            }
        });
    }
    receiver
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_negotiation_probe_is_recognized_only_for_its_connection() {
        let connection = [1, 2, 3, 4, 5, 6, 7, 8];
        let probe = version_negotiation(&connection);
        assert_eq!(probe.len(), 1200);
        assert_eq!(probe[1..5], [0x1a, 0x2a, 0x3a, 0x4a]);
        let mut reply = vec![0x80, 0, 0, 0, 0, 8];
        reply.extend_from_slice(&connection);
        reply.push(8);
        reply.extend_from_slice(&[9; 8]);
        reply.extend_from_slice(&[0, 0, 0, 1]);
        assert!(is_version_negotiation(&reply, &connection));
        assert!(!is_version_negotiation(&reply, &[0; 8]));
    }
}
