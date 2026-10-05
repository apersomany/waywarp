// The supervisor owns one instance's namespaces, forwarding, bridge, and WARP daemon.
mod bootstrap;
mod bridge;
mod control;

use crate::dataplane::{self, Frontends, Generation, Observer};
use crate::ipc::{Channel, Listener};
use crate::location::Locations;
use crate::location::geofeed::Geofeed;
use crate::notify::Notifier;
use crate::output;
use crate::protocol::{Access, Event, Failure, Plan, Progress, Status};
use crate::sandbox::{self, Private};
use crate::store::{Lock, Name};
use crate::via::Route;
use crate::warp::{self, Daemon, Monitor, State};
use anyhow::{Context, Result, anyhow, bail};
use bridge::Bridge;
use nix::sys::signal::{SigSet, Signal};
use std::net::TcpListener;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tracing::{error, info, warn};

enum Stop {
    // Holding the request channel until cleanup finishes makes EOF a shutdown acknowledgment.
    Requested(Option<Channel>),
    Failed(anyhow::Error),
}

#[derive(Clone)]
struct Stopper {
    sender: mpsc::Sender<Stop>,
    observer: Observer,
    monitor: Monitor,
}

impl Stopper {
    fn new(sender: mpsc::Sender<Stop>) -> Self {
        let observer = Observer::default();
        let changed = observer.clone();
        let monitor = Monitor::new(move || changed.control_changed());
        Self {
            sender,
            observer,
            monitor,
        }
    }

    fn request(&self, reason: Stop) {
        let _ = self.sender.send(reason);
        self.cancel();
    }

    fn cancel(&self) {
        self.observer.stop();
        self.monitor.cancel();
    }

    fn check(&self) -> Result<()> {
        if self.observer.stopped() {
            bail!("instance setup was cancelled");
        }
        Ok(())
    }
}

pub enum Reporter {
    Client(Channel),
    Foreground(Notifier),
}

impl Reporter {
    fn progress(&self, index: u8, name: Option<&Name>, progress: Progress) {
        match self {
            Self::Client(channel) => {
                output::progress(index, name, &progress);
                let _ = channel.send(&Event::Progress(progress), &[]);
            }
            Self::Foreground(_) => output::progress(index, name, &progress),
        }
    }

    fn up(&self, status: &Status) -> Result<()> {
        if !status.healthy() {
            bail!("WARP changed before the instance became ready");
        }
        match self {
            Self::Client(channel) => {
                let instance_name = status.name.as_ref().map(ToString::to_string);
                info!(target: output::LIFECYCLE_TARGET, instance = status.index,
                    instance_name = instance_name.as_deref(),
                    status = %output::summary(status), "instance ready");
                channel.send(&Event::Up(Box::new(status.clone())), &[])
            }
            Self::Foreground(notifier) => {
                output::ready(status, None)?;
                notifier.ready(&output::summary(status));
                Ok(())
            }
        }
    }

    fn failed(&self, failure: &anyhow::Error) {
        if let Self::Client(channel) = self {
            error!("setup failed: {failure:#}");
            let _ = channel.send(&Event::Failed(Failure::from(failure)), &[]);
        }
    }
}

type Report<'a> = &'a (dyn Fn(Progress) + Sync);

#[derive(Default)]
struct Observation {
    generation: Option<Generation>,
    locations: Locations,
    relay: Option<String>,
    matched: bool,
    rebootstraps: u32,
}

impl Observation {
    fn publish(
        &mut self,
        observer: &Observer,
        verified: bootstrap::Verified,
        matched: bool,
    ) -> bool {
        if !observer.confirm(verified.generation) {
            return false;
        }
        self.generation = Some(verified.generation);
        self.locations = verified.locations;
        self.matched = matched;
        true
    }

    fn state(&self, observer: &Observer, state: State, sequence: u64) -> State {
        let current = self.generation.is_some_and(|generation| {
            !observer.stopped()
                && generation == observer.generation()
                && generation.control == sequence
        });
        if state == State::Connected && !current {
            State::Degraded
        } else {
            state
        }
    }
}

struct Supervisor {
    plan: Plan,
    private: Private,
    dataplane: dataplane::Handle,
    monitor: Monitor,
    daemon: Mutex<Option<Daemon>>,
    geofeed: Option<Geofeed>,
    bridge: Option<Bridge>,
    observation: Mutex<Observation>,
}

impl Supervisor {
    fn proxy(&self) -> bool {
        matches!(self.plan.access, Access::Proxy { .. })
    }

    fn status(&self) -> Status {
        let observation = self.observation.lock().unwrap();
        let (state, sequence) = self.monitor.snapshot();
        let state = observation.state(&self.dataplane.observer, state, sequence);
        Status {
            index: self.plan.instance.index,
            name: self.plan.name.clone(),
            access: self.plan.access.clone(),
            state,
            locations: observation.locations.clone(),
            relay: observation.relay.clone(),
            matched: observation.matched && state == State::Connected,
            rebootstraps: observation.rebootstraps,
            nat: self.bridge.as_ref().and_then(Bridge::policy),
        }
    }

    fn publish(&self, verified: bootstrap::Verified, matched: bool, route: Option<&Route>) -> bool {
        let mut observation = self.observation.lock().unwrap();
        let (state, sequence) = self.monitor.snapshot();
        if state != State::Connected
            || sequence != verified.generation.control
            || !observation.publish(&self.dataplane.observer, verified, matched)
        {
            return false;
        }
        if let Some(route) = route {
            observation.relay = match route {
                Route::Direct => None,
                Route::Relay(relay) => Some(relay.label.clone()),
            };
        }
        true
    }

    fn reconcile_bridge(&self) -> Result<bool> {
        match &self.bridge {
            Some(bridge) => bridge
                .reconcile()
                .context("reconciling the bridge with WARP"),
            None => Ok(true),
        }
    }

    fn shutdown(&self) {
        self.dataplane.stop();
        self.monitor.stop();
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

struct Running {
    supervisor: Arc<Supervisor>,
    workers: Vec<JoinHandle<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.supervisor.shutdown();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        let _ = std::fs::remove_file(self.supervisor.plan.instance.control());
    }
}

pub fn detached() -> Result<()> {
    let setup = Channel::from(std::io::stdin().as_fd().try_clone_to_owned()?);
    let (plan, descriptors): (Plan, Vec<OwnedFd>) = setup.expect()?;
    let expected = if matches!(plan.access, Access::Proxy { .. }) {
        2
    } else {
        1
    };
    if descriptors.len() != expected {
        bail!("expected {expected} setup descriptors");
    }
    let mut descriptors = descriptors.into_iter();
    let lock = Lock::from(
        descriptors
            .next()
            .context("the instance lock was not passed")?,
    );
    let listener = descriptors.next().map(TcpListener::from);
    run(plan, lock, listener, Reporter::Client(setup))
}

// Isolation precedes every thread because user namespaces require a single-threaded process.
pub fn run(
    plan: Plan,
    _lock: Lock,
    listener: Option<TcpListener>,
    reporter: Reporter,
) -> Result<()> {
    let mut signals = SigSet::empty();
    for signal in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP] {
        signals.add(signal);
    }
    signals.thread_block()?;
    let (index, name) = (plan.instance.index, plan.name.clone());
    let instance_name = name.as_ref().map(ToString::to_string);
    info!(target: output::LIFECYCLE_TARGET, instance = index,
        instance_name = instance_name.as_deref(), access = %plan.access, "starting instance");
    let (sender, stopped) = mpsc::channel();
    let stop = Stopper::new(sender);
    let ready = Arc::new(AtomicBool::new(false));
    let started = (|| -> Result<Running> {
        sandbox::isolate(&plan.instance)?;
        watch(signals, &ready, &stop, &reporter)?;
        let supervisor = start(plan, listener, &stop, &|progress| {
            reporter.progress(index, name.as_ref(), progress)
        })?;
        let mut running = Running {
            supervisor: Arc::new(supervisor),
            workers: Vec::new(),
        };
        let control = running.supervisor.plan.instance.control();
        let _ = std::fs::remove_file(&control);
        let listener = Listener::bind(&control).context("binding the control socket")?;
        if let Some(worker) = bridge::watch(&running.supervisor, &stop)? {
            running.workers.push(worker);
        }
        running
            .workers
            .push(recheck_on_reconnect(&running.supervisor, &stop)?);
        running
            .workers
            .push(control::serve(listener, &running.supervisor, &stop)?);
        stop.check()?;
        ready.store(true, Ordering::SeqCst);
        reporter.up(&running.supervisor.status())?;
        Ok(running)
    })();
    let running = match started {
        Ok(running) => running,
        Err(failure) => {
            stop.cancel();
            let failure = match stopped.try_recv() {
                Ok(Stop::Requested(_channel)) => return Ok(()),
                Ok(Stop::Failed(error)) => error,
                Err(_) => failure,
            };
            reporter.failed(&failure);
            return match reporter {
                Reporter::Client(_) => Ok(()),
                Reporter::Foreground(_) => Err(failure),
            };
        }
    };
    drop(reporter);
    let reason = stopped.recv().unwrap_or(Stop::Requested(None));
    drop(running);
    match reason {
        Stop::Requested(_channel) => {
            info!(target: output::LIFECYCLE_TARGET, instance = index,
                instance_name = instance_name.as_deref(), "stopped");
            Ok(())
        }
        Stop::Failed(error) => Err(error),
    }
}

fn start(
    plan: Plan,
    listener: Option<TcpListener>,
    stop: &Stopper,
    report: Report,
) -> Result<Supervisor> {
    stop.check()?;
    if !warp::registered() {
        report(Progress::Registering);
        warp::register(plan.accept_tos, &stop.observer)?;
    }
    stop.check()?;
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
    stop.check()?;
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
                private.run(|| crate::via::interface::connect_tcp(warp::proxy_address()))
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
        stop.observer.clone(),
        move |result| {
            if !failed.observer.stopped() {
                let error = result.err().map_or_else(
                    || anyhow!("the data plane stopped"),
                    |error| error.context("the data plane failed"),
                );
                failed.request(Stop::Failed(error));
            }
        },
    )?;
    let mut supervisor = Supervisor {
        plan,
        private,
        dataplane,
        monitor: stop.monitor.clone(),
        daemon: Mutex::new(None),
        geofeed,
        bridge: None,
        observation: Mutex::new(Observation::default()),
    };
    supervisor.monitor.start(&supervisor.private)?;
    stop.check()?;
    supervisor.bridge = Bridge::attach(
        &supervisor.plan,
        &supervisor.private,
        &supervisor.dataplane.observer,
    )?;
    stop.check()?;
    report(Progress::StartingDaemon);
    let exited = stop.clone();
    let daemon = Daemon::start(&supervisor.private, &stop.observer, move |status| {
        if !exited.observer.stopped() {
            exited.request(Stop::Failed(anyhow!("warp-svc exited ({status})")));
        }
    })?;
    *supervisor.daemon.lock().unwrap() = Some(daemon);
    stop.check()?;
    let (edge, proxy) = (supervisor.plan.edge, supervisor.proxy());
    supervisor
        .private
        .run(|| warp::configure(edge, proxy, &stop.observer))?;
    bootstrap::bootstrap(&supervisor, report)?;
    stop.check()?;
    Ok(supervisor)
}

fn recheck_on_reconnect(supervisor: &Arc<Supervisor>, stop: &Stopper) -> Result<JoinHandle<()>> {
    let (supervisor, failed) = (Arc::clone(supervisor), stop.clone());
    Ok(thread::Builder::new()
        .name("recheck".into())
        .spawn(move || {
            let observer = &supervisor.dataplane.observer;
            while !observer.stopped() {
                let Some(generation) = observer.pending(Duration::from_secs(3600)) else {
                    continue;
                };
                if let Err(error) = bootstrap::recheck(&supervisor) {
                    failed.request(Stop::Failed(error));
                    return;
                }
                observer.wait_change(generation, Duration::from_secs(1));
            }
        })?)
}

fn watch(
    signals: SigSet,
    ready: &Arc<AtomicBool>,
    stop: &Stopper,
    reporter: &Reporter,
) -> Result<()> {
    let signal_stop = stop.clone();
    thread::Builder::new()
        .name("signals".into())
        .spawn(move || match signals.wait() {
            Ok(signal) => {
                info!(?signal, "stopping");
                signal_stop.request(Stop::Requested(None));
            }
            Err(error) => signal_stop.request(Stop::Failed(error.into())),
        })?;
    if let Reporter::Client(channel) = reporter {
        let client = channel.try_clone()?;
        client.set_timeout(Some(Duration::from_millis(100)))?;
        let (ready, stop) = (Arc::clone(ready), stop.clone());
        thread::Builder::new()
            .name("setup-client".into())
            .spawn(move || {
                while !ready.load(Ordering::SeqCst) && !stop.observer.stopped() {
                    match client.receive::<serde_json::Value>() {
                        Ok(Some(_)) => {}
                        Err(error)
                            if error.downcast_ref::<nix::errno::Errno>()
                                == Some(&nix::errno::Errno::EAGAIN) => {}
                        _ => {
                            // Ready may have been published while this receive was blocked.
                            if !ready.load(Ordering::SeqCst) && !stop.observer.stopped() {
                                warn!("setup abandoned by the client");
                                stop.request(Stop::Requested(None));
                            }
                            return;
                        }
                    }
                }
            })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_wakes_waiters_and_holds_the_shutdown_acknowledgment() {
        let (sender, reasons) = mpsc::channel();
        let stop = Stopper::new(sender);
        let (request, client) = Channel::pair().unwrap();
        client.set_timeout(Some(Duration::from_millis(10))).unwrap();
        stop.request(Stop::Requested(Some(request)));
        let reason = reasons.recv().unwrap();
        assert!(stop.observer.stopped());
        assert!(
            stop.monitor
                .wait(0, Duration::from_secs(60), |_| true)
                .is_none()
        );
        assert!(client.receive::<serde_json::Value>().is_err());
        drop(reason);
        assert!(client.receive::<serde_json::Value>().unwrap().is_none());
    }

    #[test]
    fn stale_results_are_not_published_or_reported_healthy() {
        let observer = Observer::default();
        let generation = observer.generation();
        let mut observation = Observation::default();
        observer.control_changed();
        assert!(!observation.publish(
            &observer,
            bootstrap::Verified {
                generation,
                locations: Locations {
                    edge: "OLD".into(),
                    ..Locations::default()
                },
            },
            true
        ));
        assert!(observation.locations.edge.is_empty());
        let generation = observer.generation();
        assert!(observation.publish(
            &observer,
            bootstrap::Verified {
                generation,
                locations: Locations {
                    edge: "NEW".into(),
                    ..Locations::default()
                },
            },
            true
        ));
        assert_eq!(
            observation.state(&observer, State::Connected, generation.control),
            State::Connected
        );
        // Fence the interval between the daemon publishing its state and notifying the observer.
        assert_eq!(
            observation.state(&observer, State::Connected, generation.control + 1),
            State::Degraded
        );
        observer.link_changed();
        assert_eq!(
            observation.state(&observer, State::Connected, generation.control),
            State::Degraded
        );
        observer.stop();
        assert!(!observation.publish(
            &observer,
            bootstrap::Verified {
                generation: observer.generation(),
                locations: Locations::default(),
            },
            true
        ));
    }
}
