// The supervisor owns one instance for its whole life: namespaces, TUN, veth, data plane, and
// warp-svc all go away with it.
mod bootstrap;
mod bridge;
mod control;

use crate::dataplane::{self, Frontends};
use crate::ipc::{Channel, Listener};
use crate::location::Locations;
use crate::location::geofeed::Geofeed;
use crate::notify::Notifier;
use crate::protocol::{Access, Event, Plan, Status};
use crate::sandbox::{self, Private};
use crate::store::Lock;
use crate::warp::{self, Daemon, Monitor, State};
use anyhow::{Context, Result, bail};
use bridge::Bridge;
use nix::sys::signal::{SigSet, Signal};
use std::net::TcpListener;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;
use tracing::{error, info, warn};

enum Stop {
    Requested,
    Failed(String),
}

// Where setup progress goes: to the waiting `up` client, or to the log in the foreground.
pub enum Reporter {
    Client(Channel),
    Foreground(Notifier),
}

impl Reporter {
    fn progress(&self, message: String) {
        info!("{message}");
        if let Self::Client(channel) = self {
            let _ = channel.send(&Event::Progress(message), &[]);
        }
    }

    fn up(&self, status: &Status) -> Result<()> {
        info!("{status}");
        match self {
            Self::Client(channel) => channel.send(&Event::Up(Box::new(status.clone())), &[]),
            Self::Foreground(notifier) => {
                println!("{status}");
                notifier.ready(&status.to_string());
                Ok(())
            }
        }
    }

    fn failed(&self, reason: String) {
        error!("setup failed: {reason}");
        if let Self::Client(channel) = self {
            let _ = channel.send(&Event::Failed(reason), &[]);
        }
    }
}

type Report<'a> = &'a (dyn Fn(String) + Sync);

// Everything an instance owns at runtime. Background threads share it until the process exits,
// so `shutdown` is explicit; anything it misses the kernel reclaims when the process ends.
struct Supervisor {
    plan: Plan,
    private: Private,
    dataplane: dataplane::Handle,
    monitor: Monitor,
    daemon: Mutex<Option<Daemon>>,
    geofeed: Option<Geofeed>,
    bridge: Option<Bridge>,
    status: Mutex<Status>,
}

impl Supervisor {
    fn proxy(&self) -> bool {
        matches!(self.plan.access, Access::Proxy { .. })
    }

    fn status(&self) -> Status {
        let mut status = self.status.lock().unwrap().clone();
        status.state = self.monitor.state();
        status.nat = self.bridge.as_ref().and_then(Bridge::policy);
        status
    }

    fn reconcile_bridge(&self) -> Result<()> {
        match &self.bridge {
            Some(bridge) => bridge
                .reconcile()
                .context("reconciling the bridge with WARP"),
            None => Ok(()),
        }
    }

    fn shutdown(&self) {
        self.monitor.stop();
        self.dataplane.stop();
        self.daemon.lock().unwrap().take();
        if let Some(bridge) = &self.bridge {
            bridge.stop();
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// Entry point of a detached supervisor: the plan, the instance lock, and any proxy listener
// arrive on stdin from `up`.
pub fn detached() -> Result<()> {
    let setup = Channel::from(std::io::stdin().as_fd().try_clone_to_owned()?);
    let (plan, descriptors): (Plan, Vec<OwnedFd>) = setup.expect()?;
    let mut descriptors = descriptors.into_iter();
    let lock = Lock::from(
        descriptors
            .next()
            .context("the instance lock was not passed")?,
    );
    let listener = descriptors.next().map(TcpListener::from);
    run(plan, lock, listener, Reporter::Client(setup))
}

// Must start while the process is single-threaded; returns once the instance stops.
pub fn run(
    plan: Plan,
    _lock: Lock,
    listener: Option<TcpListener>,
    reporter: Reporter,
) -> Result<()> {
    // Blocked before any thread starts so every thread inherits the mask and one waits for them.
    let mut signals = SigSet::empty();
    for signal in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP] {
        signals.add(signal);
    }
    signals.thread_block()?;
    info!(index = plan.instance.index, access = %plan.access, "starting instance");
    let control = plan.instance.control();
    let (stop, stopped) = mpsc::channel();
    let serving = Arc::new(AtomicBool::new(false));
    // User namespaces can only be entered while single-threaded, so isolation precedes every thread.
    let started = sandbox::isolate(&plan.instance)
        .and_then(|()| watch(signals, &serving, &stop, &reporter))
        .and_then(|()| start(plan, listener, &stop, &|message| reporter.progress(message)))
        .and_then(|supervisor| {
            let _ = std::fs::remove_file(&control);
            let listener = Listener::bind(&control).context("binding the control socket")?;
            Ok((supervisor, listener))
        });
    let (supervisor, listener) = match started {
        Ok(started) => started,
        Err(failure) => {
            let reason = format!("{failure:#}");
            reporter.failed(reason.clone());
            return match reporter {
                Reporter::Client(_) => Ok(()),
                Reporter::Foreground(_) => Err(failure),
            };
        }
    };
    serving.store(true, Ordering::SeqCst);
    reporter.up(&supervisor.status())?;
    drop(reporter);
    let supervisor = Arc::new(supervisor);
    bridge::watch(&supervisor, &stop)?;
    recheck_on_reconnect(&supervisor, &stop)?;
    control::serve(listener, &supervisor, &stop)?;
    let reason = stopped.recv().unwrap_or(Stop::Requested);
    let _ = std::fs::remove_file(&control);
    supervisor.shutdown();
    match reason {
        Stop::Requested => {
            info!("instance stopped");
            Ok(())
        }
        Stop::Failed(reason) => {
            error!("instance failed: {reason}");
            bail!("{reason}")
        }
    }
}

fn start(
    plan: Plan,
    listener: Option<TcpListener>,
    stop: &mpsc::Sender<Stop>,
    report: Report,
) -> Result<Supervisor> {
    if !warp::registered() {
        report("registering a WARP device".into());
        warp::register(plan.accept_tos)?;
    }
    let geofeed = match Geofeed::load(&plan.instance.store) {
        Ok(geofeed) => Some(geofeed),
        Err(error) if plan.location.constrains_geo() => {
            return Err(error.context("loading Cloudflare's location data"));
        }
        Err(error) => {
            warn!("geo fields are unavailable: {error:#}");
            None
        }
    };
    plan.location.validate(geofeed.as_ref())?;
    let private = Private::create()?;
    let tun = private.run(sandbox::tun)?;
    let redirect = if plan.redirect_tcp {
        Some(private.run(sandbox::redirect_tcp)?)
    } else {
        None
    };
    let proxy = match (&plan.access, listener) {
        (Access::Proxy { .. }, Some(listener)) => {
            let private = private.clone();
            let connect: dataplane::Connect = Box::new(move || {
                private.run(|| Ok(std::net::TcpStream::connect(warp::proxy_address())?))
            });
            Some((listener, connect))
        }
        (Access::Proxy { .. }, None) => {
            bail!("the proxy listener was not passed to the supervisor")
        }
        (Access::Bridge { .. }, _) => None,
    };
    let failed = stop.clone();
    let dataplane = dataplane::start(
        tun,
        plan.interface.clone(),
        Frontends { proxy, redirect },
        move |result| {
            let reason = result.err().map_or_else(
                || "the data plane stopped".into(),
                |error| format!("the data plane failed: {error:#}"),
            );
            let _ = failed.send(Stop::Failed(reason));
        },
    )?;
    let status = Status {
        index: plan.instance.index,
        name: plan.name.clone(),
        access: plan.access.clone(),
        state: State::Unknown,
        locations: Locations::default(),
        relay: None,
        matched: true,
        rebootstraps: 0,
        nat: None,
    };
    let monitor = Monitor::start(&private)?;
    let mut supervisor = Supervisor {
        plan,
        private,
        dataplane,
        monitor,
        daemon: Mutex::new(None),
        geofeed,
        bridge: None,
        status: Mutex::new(status),
    };
    supervisor.bridge = Bridge::attach(&supervisor.plan, &supervisor.private)?;
    report("starting warp-svc".into());
    let exited = stop.clone();
    let daemon = Daemon::start(&supervisor.private, move |status| {
        let _ = exited.send(Stop::Failed(format!("warp-svc exited ({status})")));
    })?;
    *supervisor.daemon.lock().unwrap() = Some(daemon);
    let (edge, proxy) = (supervisor.plan.edge, supervisor.proxy());
    supervisor.private.run(|| warp::configure(edge, proxy))?;
    bootstrap::bootstrap(&supervisor, report)?;
    supervisor.reconcile_bridge()?;
    Ok(supervisor)
}

// Rechecks required locations after every Connected event. Migration is checked synchronously by
// the bootstrap, so no timer is needed.
fn recheck_on_reconnect(supervisor: &Arc<Supervisor>, stop: &mpsc::Sender<Stop>) -> Result<()> {
    let (supervisor, failed) = (Arc::clone(supervisor), stop.clone());
    thread::Builder::new()
        .name("recheck".into())
        .spawn(move || {
            let monitor = &supervisor.monitor;
            let mut since = monitor.sequence();
            while !monitor.stopped() {
                let connected = monitor.wait(since, Duration::from_secs(3600), |state| {
                    *state == State::Connected
                });
                if connected.is_none() {
                    continue;
                }
                // warp-svc recreates its link when it reconnects, removing the bridge route.
                // Restore it before the recheck, whose location probes can take a while, and
                // again after it, since a rebootstrap reconnects without raising this wait.
                let routed = supervisor
                    .reconcile_bridge()
                    .and_then(|()| bootstrap::recheck(&supervisor))
                    .and_then(|()| supervisor.reconcile_bridge());
                if let Err(error) = routed {
                    let _ = failed.send(Stop::Failed(format!("{error:#}")));
                    return;
                }
                // Events raised by a rebootstrap describe the new tunnel, which is already checked.
                since = monitor.sequence();
            }
        })?;
    Ok(())
}

// Before the instance is up, a signal or the `up` client going away abandons setup by exiting:
// the kernel then releases the namespaces, TUN, and veth, and warp-svc dies with its owner thread.
fn watch(
    signals: SigSet,
    serving: &Arc<AtomicBool>,
    stop: &mpsc::Sender<Stop>,
    reporter: &Reporter,
) -> Result<()> {
    let (serving_signal, signal_stop) = (Arc::clone(serving), stop.clone());
    thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            loop {
                let signal = signals.wait();
                if !serving_signal.load(Ordering::SeqCst) {
                    warn!(?signal, "setup interrupted");
                    std::process::exit(1);
                }
                info!(?signal, "stopping");
                let _ = signal_stop.send(Stop::Requested);
            }
        })?;
    if let Reporter::Client(channel) = reporter {
        let client = channel.try_clone()?;
        let serving = Arc::clone(serving);
        thread::Builder::new()
            .name("setup-client".into())
            .spawn(move || {
                while let Ok(Some(_)) = client.receive::<serde_json::Value>() {}
                if !serving.load(Ordering::SeqCst) {
                    warn!("setup abandoned by the client");
                    std::process::exit(1);
                }
            })?;
    }
    Ok(())
}
