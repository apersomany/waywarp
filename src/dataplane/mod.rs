// One event loop per instance carries every packet and stream between the private namespace and
// the host.
mod observe;
mod packet;
mod quic;
mod tcp;
mod udp;

pub use observe::{Generation, Observer};

use crate::via::Route;
use crate::via::interface::Interface;
use anyhow::{Context, Result, bail};
use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token, Waker};
use nix::sys::socket::{SockaddrIn, getsockopt, sockopt::OriginalDst};
use std::io::ErrorKind;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
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
    Block(mpsc::Sender<()>),
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
    pub observer: Observer,
    owner: Mutex<Option<JoinHandle<()>>>,
}

impl Handle {
    fn apply(&self, command: impl FnOnce(mpsc::Sender<()>) -> Command) -> Result<()> {
        if self.observer.stopped() {
            bail!("the data plane stopped");
        }
        let (ready, answer) = mpsc::channel();
        self.commands
            .send(command(ready))
            .map_err(|_| anyhow::anyhow!("the data plane stopped"))?;
        self.waker.wake()?;
        answer
            .recv_timeout(Duration::from_secs(5))
            .context("waiting for the data plane")
    }

    // Acknowledge blocking before asking WARP to abandon the preceding connection.
    pub fn block(&self) -> Result<()> {
        self.apply(Command::Block)
    }

    pub fn stop(&self) {
        self.observer.stop();
        let _ = self.commands.send(Command::Stop);
        let _ = self.waker.wake();
        let owner = self.owner.lock().unwrap().take();
        if let Some(owner) = owner {
            let _ = owner.join();
        }
    }

    // Runs on the host: finish paced authentication before warp-svc's handshake timer starts.
    // Then wait until the event loop has installed the route and its prepared edge association.
    pub fn open(&self, route: Route, edge: std::net::SocketAddrV4) -> Result<()> {
        if self.observer.stopped() {
            bail!("the data plane stopped");
        }
        let routing = udp::Routing::prepare(&self.interface, route, edge, &self.observer)?;
        self.apply(|ready| Command::Open(routing, ready))
            .context("installing the connection route")
    }

    // Acknowledges the installed path, not tunnel health. The supervisor verifies the tunnel
    // with fresh probes; a UDP reply alone cannot distinguish migration from a failed handshake.
    pub fn migrate(&self) -> Result<()> {
        self.apply(Command::Migrate)
            .context("installing the direct path")
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
    observer: Observer,
    on_stop: impl FnOnce(Result<()>) + Send + 'static,
) -> Result<Handle> {
    let poll = Poll::new()?;
    let waker = Arc::new(Waker::new(poll.registry(), WAKE)?);
    let flows = Flows::new(
        tun,
        interface.clone(),
        Arc::clone(&waker),
        FLOWS,
        observer.clone(),
    );
    flows.register(poll.registry(), TUN)?;
    let (commands, queue) = mpsc::channel();
    let physical = interface.clone();
    let stopped = observer.clone();
    let owner = std::thread::Builder::new()
        .name("dataplane".into())
        .spawn(move || on_stop(run(poll, flows, interface, frontends, &queue, &stopped)))?;
    Ok(Handle {
        commands,
        waker,
        interface: physical,
        observer,
        owner: Mutex::new(Some(owner)),
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
    observer: &Observer,
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
        if observer.stopped() {
            return Ok(());
        }
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
                            Command::Block(ready) => {
                                flows.reset(registry, udp::Routing::Blocked);
                                let _ = ready.send(());
                            }
                            Command::Open(routing, ready) => {
                                flows.reset(registry, routing);
                                let _ = ready.send(());
                            }
                            Command::Migrate(ready) => {
                                flows.migrate(registry);
                                let _ = ready.send(());
                            }
                            Command::Stop => return Ok(()),
                        }
                    }
                }
                TUN => flows
                    .tun_ready(registry)
                    .context("reading the private uplink")?,
                PROXY => {
                    if let Some((listener, connect)) = &mut proxy {
                        accept(
                            || listener.accept().map(|(stream, _)| stream),
                            &mut splices,
                            registry,
                            observer,
                            |_| {
                                let stream = connect()?;
                                stream.set_nonblocking(true)?;
                                Ok(TcpStream::from_std(stream))
                            },
                        );
                    }
                }
                REDIRECT => {
                    if let Some(listener) = &mut redirect {
                        accept(
                            || listener.accept().map(|(stream, _)| stream),
                            &mut splices,
                            registry,
                            observer,
                            |incoming| {
                                let original = SockaddrIn::from(getsockopt(incoming, OriginalDst)?);
                                Ok(TcpStream::from_std(interface.tcp(original.into())?))
                            },
                        );
                    }
                }
                token if flows.owns(token) => flows
                    .socket_ready(token)
                    .context("writing the private uplink")?,
                token if splices.owns(token) => {
                    splices.ready(registry, token, || observer.stopped())
                }
                _ => {}
            }
        }
        flows.sweep(registry);
    }
}

// Splices every pending accepted connection to the stream `outgoing` opens for it.
fn accept(
    mut incoming: impl FnMut() -> std::io::Result<TcpStream>,
    splices: &mut Splices,
    registry: &mio::Registry,
    observer: &Observer,
    outgoing: impl Fn(&TcpStream) -> Result<TcpStream>,
) {
    while !observer.stopped() {
        let incoming = match incoming() {
            Ok(incoming) => incoming,
            Err(error) if error.kind() == ErrorKind::WouldBlock => return,
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::Interrupted | ErrorKind::ConnectionAborted
                ) =>
            {
                continue;
            }
            // Resource exhaustion affects this drain, not the listener's lifetime.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelling_route_preparation_does_not_wait_for_the_authentication_lock() {
        let directory = crate::test_support::TempDirectory::new("route-cancel");
        let limiter_path = directory.path.join("limiter");
        let limiter = std::fs::File::create(&limiter_path).unwrap();
        limiter.lock().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let std::net::SocketAddr::V4(address) = listener.local_addr().unwrap() else {
            unreachable!()
        };
        drop(listener);
        let poll = Poll::new().unwrap();
        let (commands, _queue) = mpsc::channel();
        let observer = Observer::default();
        let handle = Handle {
            commands,
            waker: Arc::new(Waker::new(poll.registry(), WAKE).unwrap()),
            interface: Interface::Named("lo".into()),
            observer: observer.clone(),
            owner: Mutex::new(None),
        };
        let route = Route::Relay(crate::via::Relay {
            label: "test".into(),
            server: address,
            login: Some(crate::via::Login {
                username: "user".into(),
                password: "password".into(),
                auth_limit: Some(limiter_path.clone()),
            }),
        });
        let (finished, result) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            finished.send(handle.open(route, address)).unwrap();
        });
        assert!(matches!(
            result.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        observer.stop();
        let cancelled = result.recv_timeout(Duration::from_secs(1));
        drop(limiter);
        worker.join().unwrap();
        assert_eq!(
            cancelled.unwrap().unwrap_err().root_cause().to_string(),
            "relay operation was cancelled"
        );
        assert!(std::fs::read(limiter_path).unwrap().is_empty());
    }

    #[test]
    fn transient_accept_errors_do_not_abandon_pending_clients() {
        let ingress = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let _client = std::net::TcpStream::connect(ingress.local_addr().unwrap()).unwrap();
        let (incoming, _) = ingress.accept().unwrap();
        incoming.set_nonblocking(true).unwrap();
        let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let poll = Poll::new().unwrap();
        let mut splices = Splices::new(SPLICES);
        let observer = Observer::default();
        let mut outcomes = [
            Err(ErrorKind::Interrupted.into()),
            Err(ErrorKind::ConnectionAborted.into()),
            Ok(TcpStream::from_std(incoming)),
            Err(ErrorKind::WouldBlock.into()),
        ]
        .into_iter();
        let mut accepted = 0;
        let connected = std::cell::Cell::new(0);
        accept(
            || {
                accepted += 1;
                outcomes
                    .next()
                    .expect("must stop once the queue is drained")
            },
            &mut splices,
            poll.registry(),
            &observer,
            |_| {
                connected.set(connected.get() + 1);
                Ok(TcpStream::from_std(crate::via::interface::connect_tcp(
                    target.local_addr()?,
                )?))
            },
        );
        assert_eq!(accepted, 4);
        assert_eq!(connected.get(), 1);
    }
}
