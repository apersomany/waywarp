// One event loop per instance carries every packet and stream between the private namespace and
// the host.
mod packet;
mod tcp;
mod udp;

use crate::via::Route;
use crate::via::interface::Interface;
use anyhow::{Context, Result, anyhow, bail};
use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token, Waker};
use nix::sys::socket::{SockaddrIn, getsockopt, sockopt::OriginalDst};
use std::io::ErrorKind;
use std::sync::{Arc, mpsc};
use std::time::Duration;
use tcp::Splices;
use tracing::{debug, warn};
use udp::Flows;

const WAKE: Token = Token(0);
const TUN: Token = Token(1);
const PROXY: Token = Token(2);
const REDIRECT: Token = Token(3);
const FLOWS: usize = 16;
const SPLICES: usize = FLOWS + udp::MAX_FLOWS * 2;

enum Command {
    Block,
    Open(udp::Routing, mpsc::Sender<()>),
    Migrate(mpsc::Sender<()>),
    Stop,
}

// Where the host listener's connections go: warp-svc's proxy port, reached from inside the
// private namespace.
pub type Connect = Box<dyn Fn() -> Result<std::net::TcpStream> + Send>;

pub struct Frontends {
    pub proxy: Option<(std::net::TcpListener, Connect)>,
    // Locally originated TCP inside the namespace, redirected by nftables for Team registrations.
    pub redirect: Option<std::net::TcpListener>,
}

pub struct Handle {
    commands: mpsc::Sender<Command>,
    waker: Arc<Waker>,
    interface: Interface,
}

impl Handle {
    fn send(&self, command: Command) {
        let _ = self.commands.send(command);
        let _ = self.waker.wake();
    }

    // Drops every flow and keeps new ones from leaving.
    pub fn block(&self) {
        self.send(Command::Block);
    }

    pub fn stop(&self) {
        self.send(Command::Stop);
    }

    // Runs on the host: finish paced authentication before warp-svc's handshake timer starts.
    // Then wait until the event loop has installed the route and its prepared edge association.
    pub fn open(&self, route: Route, edge: std::net::SocketAddrV4) -> Result<()> {
        let routing = udp::Routing::prepare(&self.interface, route, edge)?;
        let (ready, answer) = mpsc::channel();
        self.send(Command::Open(routing, ready));
        answer
            .recv_timeout(Duration::from_secs(5))
            .context("waiting for the data plane to install the route")
    }

    // Moves every flow to its direct socket and waits until the edge answers on one.
    pub fn migrate(&self, timeout: Duration) -> Result<()> {
        let (answered, answer) = mpsc::channel();
        self.send(Command::Migrate(answered));
        match answer.recv_timeout(timeout) {
            Ok(()) => Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                bail!("the edge did not answer on the direct path")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(anyhow!("the data plane stopped")),
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn start(
    tun: std::fs::File,
    interface: Interface,
    frontends: Frontends,
    on_stop: impl FnOnce(Result<()>) + Send + 'static,
) -> Result<Handle> {
    let poll = Poll::new()?;
    let waker = Arc::new(Waker::new(poll.registry(), WAKE)?);
    let flows = Flows::new(tun, interface.clone(), Arc::clone(&waker), FLOWS);
    flows.register(poll.registry(), TUN)?;
    let (commands, queue) = mpsc::channel();
    let physical = interface.clone();
    std::thread::Builder::new()
        .name("dataplane".into())
        .spawn(move || on_stop(run(poll, flows, interface, frontends, &queue)))?;
    Ok(Handle {
        commands,
        waker,
        interface: physical,
    })
}

fn listen(poll: &Poll, listener: std::net::TcpListener, token: Token) -> Result<TcpListener> {
    listener.set_nonblocking(true)?;
    let mut listener = TcpListener::from_std(listener);
    poll.registry()
        .register(&mut listener, token, Interest::READABLE)?;
    Ok(listener)
}

fn run(
    mut poll: Poll,
    mut flows: Flows,
    interface: Interface,
    frontends: Frontends,
    commands: &mpsc::Receiver<Command>,
) -> Result<()> {
    let mut proxy = match frontends.proxy {
        Some((listener, connect)) => Some((listen(&poll, listener, PROXY)?, connect)),
        None => None,
    };
    let mut redirect = match frontends.redirect {
        Some(listener) => Some(listen(&poll, listener, REDIRECT)?),
        None => None,
    };
    let mut splices = Splices::new(SPLICES);
    let mut events = Events::with_capacity(512);
    loop {
        match poll.poll(&mut events, Some(Duration::from_secs(5))) {
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            result => result?,
        }
        let registry = poll.registry();
        for event in &events {
            match event.token() {
                WAKE => {
                    flows.associations_ready(registry);
                    for command in commands.try_iter() {
                        match command {
                            Command::Block => flows.reset(registry, udp::Routing::Blocked),
                            Command::Open(routing, ready) => {
                                flows.reset(registry, routing);
                                let _ = ready.send(());
                            }
                            Command::Migrate(answered) => flows.migrate(registry, answered),
                            Command::Stop => return Ok(()),
                        }
                    }
                }
                TUN => flows.tun_ready(registry),
                PROXY => {
                    if let Some((listener, connect)) = &mut proxy {
                        accept(listener, &mut splices, registry, |_| {
                            let stream = connect()?;
                            stream.set_nonblocking(true)?;
                            Ok(TcpStream::from_std(stream))
                        });
                    }
                }
                REDIRECT => {
                    if let Some(listener) = &mut redirect {
                        accept(listener, &mut splices, registry, |incoming| {
                            let original = SockaddrIn::from(getsockopt(incoming, OriginalDst)?);
                            Ok(TcpStream::from_std(interface.tcp(original.into())?))
                        });
                    }
                }
                token if flows.owns(token) => flows.socket_ready(token),
                token if splices.owns(token) => splices.ready(registry, token),
                _ => {}
            }
        }
        flows.sweep(registry);
    }
}

// Splices every pending connection on `listener` to the stream `outgoing` opens for it.
fn accept(
    listener: &mut TcpListener,
    splices: &mut Splices,
    registry: &mio::Registry,
    outgoing: impl Fn(&TcpStream) -> Result<TcpStream>,
) {
    loop {
        let incoming = match listener.accept() {
            Ok((incoming, _)) => incoming,
            Err(error) if error.kind() == ErrorKind::WouldBlock => return,
            // Aborted handshakes and descriptor exhaustion affect one client, not the listener.
            Err(error) => {
                warn!(%error, "accepting a TCP connection failed");
                return;
            }
        };
        let result =
            outgoing(&incoming).and_then(|outgoing| splices.insert(registry, incoming, outgoing));
        if let Err(error) = result {
            debug!(error = format!("{error:#}"), "TCP connection failed");
        }
    }
}
