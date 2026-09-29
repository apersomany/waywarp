// Drives warp-svc through warp-cli, always inside the calling thread's namespace.
mod daemon;
mod monitor;

pub use daemon::{Daemon, register, registered};
pub use monitor::{Monitor, State};

use crate::tool;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

// The TUN warp-svc creates in warp mode, and the port it listens on in proxy mode.
pub const LINK: &str = "CloudflareWARP";
pub const PROXY_PORT: u16 = 40000;

pub fn proxy_address() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, PROXY_PORT))
}

pub fn cli(arguments: &[&str]) -> Result<String> {
    let arguments: Vec<_> = std::iter::once("--accept-tos")
        .chain(arguments.iter().copied())
        .collect();
    tool::run("warp-cli", &arguments)
}

#[derive(Deserialize)]
struct Settings {
    settings: Effective,
}

#[derive(Deserialize)]
struct Effective {
    operation_mode: String,
    warp_tunnel_protocol: String,
}

// Changes only what differs, since an organization's policy can lock settings it already fixes.
pub fn configure(edge: SocketAddrV4, proxy: bool) -> Result<()> {
    let current: Settings = serde_json::from_str(&cli(&["--json", "settings"])?)?;
    let current = current.settings;
    if !current.warp_tunnel_protocol.eq_ignore_ascii_case("masque") {
        cli(&["tunnel", "protocol", "set", "MASQUE"])?;
    }
    cli(&["tunnel", "endpoint", "set", &edge.to_string()])?;
    if proxy {
        cli(&["proxy", "port", &PROXY_PORT.to_string()])?;
    }
    let (mode, suitable) = if proxy {
        ("proxy", current.operation_mode == "proxy")
    } else {
        (
            "warp",
            matches!(current.operation_mode.as_str(), "warp" | "warp+doh"),
        )
    };
    if !suitable {
        cli(&["mode", mode]).with_context(|| {
            format!(
                "switching WARP from {} to {mode} mode",
                current.operation_mode
            )
        })?;
    }
    Ok(())
}

#[derive(Deserialize)]
struct Tunnel {
    warp_is_on: bool,
    protocol: String,
    edge: Option<Edge>,
}

#[derive(Deserialize)]
struct Edge {
    colo: String,
}

pub fn tunnel(proxy: bool) -> Result<String> {
    let tunnel: Tunnel = serde_json::from_str(&cli(&["--json", "tunnel", "stats"])?)?;
    if !tunnel.warp_is_on || !tunnel.protocol.starts_with("MASQUE") {
        bail!("tunnel protocol is {}, not MASQUE", tunnel.protocol);
    }
    let colo = tunnel
        .edge
        .map(|edge| edge.colo.to_ascii_uppercase())
        .filter(|colo| !colo.is_empty())
        .context("WARP reported no edge colo")?;
    if proxy && TcpStream::connect_timeout(&proxy_address(), Duration::from_millis(500)).is_err() {
        bail!("WARP proxy port is not accepting connections");
    }
    Ok(colo)
}

// Disconnects and waits for the daemon to confirm, so a previous session cannot migrate onto the
// next attempt. A daemon that already gave up emits nothing on disconnect, so it counts as idle.
pub fn disconnect(monitor: &Monitor) -> Result<()> {
    let idle = |state: &State| matches!(state, State::Disconnected | State::Unable);
    let since = monitor.sequence();
    cli(&["disconnect"])?;
    if idle(&monitor.state()) || monitor.wait(since, Duration::from_secs(10), idle).is_some() {
        return Ok(());
    }
    bail!(
        "WARP did not disconnect within 10 s (last state: {})",
        monitor.state()
    )
}

// Connects and waits for a healthy tunnel, then reads its colo.
pub fn connect(monitor: &Monitor, proxy: bool, timeout: Duration) -> Result<String> {
    let since = monitor.sequence();
    cli(&["connect"])?;
    if monitor
        .wait(since, timeout, |state| *state == State::Connected)
        .is_none()
    {
        bail!(
            "no connection within {} s (last state: {})",
            timeout.as_secs(),
            monitor.state()
        );
    }
    // The proxy port can open slightly after WARP reports the tunnel.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match tunnel(proxy) {
            Ok(colo) => return Ok(colo),
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
}
