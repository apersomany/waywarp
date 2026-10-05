// Connects WARP through each --via entry in order until the tunnel satisfies --location.
use super::{Report, Supervisor};
use crate::dataplane::{Generation, Observer};
use crate::location::Locations;
use crate::location::probe::{self, Family, Probe};
use crate::output;
use crate::protocol::{Failure, Progress};
use crate::via::{self, Route, ping};
use crate::warp::{self, State};
use anyhow::{Context, Result, bail};
use std::time::{Duration, Instant};
use tracing::{info, warn};

const CONNECT: Duration = Duration::from_secs(45);
const PROBE: Duration = Duration::from_secs(15);
const QUIET: Duration = Duration::from_millis(250);
const RETRY: Duration = Duration::from_millis(100);

pub(super) struct Verified {
    pub locations: Locations,
    pub generation: Generation,
}

pub(super) fn bootstrap(supervisor: &Supervisor, report: Report) -> Result<()> {
    let plan = &supervisor.plan;
    let mut entries = via::expand(&plan.via, &plan.credentials, plan.mudfish_port);
    let mut failures = Vec::new();
    loop {
        if supervisor.dataplane.observer.stopped() {
            bail!("bootstrap was cancelled");
        }
        let Some((entry, routes)) = entries.next() else {
            return Err(connection_failure(&failures));
        };
        let routes = match routes {
            Ok(routes) => routes,
            Err(error) => {
                report(Progress::AttemptFailed {
                    route: entry.to_string(),
                    failure: Failure::from(&error),
                });
                failures.push(format!("{entry}: {error:#}"));
                continue;
            }
        };
        if attempt_routes(supervisor, routes, &mut failures, report)? {
            return Ok(());
        }
    }
}

// Nodes within one via entry are pinged four at a time. Later entries stay unexpanded until
// this one fails; a reported colo only orders attempts, since the hint can be wrong.
fn attempt_routes(
    supervisor: &Supervisor,
    routes: Vec<Route>,
    failures: &mut Vec<String>,
    report: Report,
) -> Result<bool> {
    let plan = &supervisor.plan;
    let mut relays = Vec::new();
    for route in routes {
        if supervisor.dataplane.observer.stopped() {
            bail!("bootstrap was cancelled");
        }
        match route {
            Route::Relay(relay) => relays.push(relay),
            Route::Direct => return Ok(attempt(supervisor, &route, failures, report)),
        }
    }
    let mut deferred = Vec::new();
    for (relay, ping) in ping::stream(
        &plan.interface,
        relays,
        plan.edge,
        &supervisor.dataplane.observer,
    ) {
        if supervisor.dataplane.observer.stopped() {
            bail!("bootstrap was cancelled");
        }
        let ping = match ping {
            Ok(ping) => ping,
            Err(error) => {
                report(Progress::RelayProbeFailed {
                    relay: relay.label.clone(),
                    failure: Failure::from(&error),
                });
                failures.push(format!("via {}: {error:#}", relay.label));
                continue;
            }
        };
        report(Progress::RelayProbed {
            relay: relay.label.clone(),
            millis: ping.rtt.as_millis(),
            colo: ping.colo.clone(),
        });
        let plausible = ping
            .colo
            .as_deref()
            .is_none_or(|colo| plan.location.plausible(colo, supervisor.geofeed.as_ref()));
        if !plausible {
            deferred.push((ping.rtt, relay));
            continue;
        }
        if attempt(supervisor, &Route::Relay(relay), failures, report) {
            return Ok(true);
        }
    }
    deferred.sort_by_key(|(rtt, _)| *rtt);
    for (_, relay) in deferred {
        if supervisor.dataplane.observer.stopped() {
            bail!("bootstrap was cancelled");
        }
        if attempt(supervisor, &Route::Relay(relay), failures, report) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn connection_failure(failures: &[String]) -> anyhow::Error {
    let omitted = failures.len().saturating_sub(3);
    let mut message = format!(
        "could not connect through any route ({} failed {})",
        failures.len(),
        if failures.len() == 1 {
            "attempt"
        } else {
            "attempts"
        },
    );
    for failure in &failures[omitted..] {
        message.push('\n');
        message.push_str(failure);
    }
    if omitted > 0 {
        message.push_str(&format!(
            "\n{omitted} earlier failures omitted; rerun with WAYWARP_LOG=debug for relay probe details"
        ));
    }
    anyhow::Error::msg(message)
}

fn attempt(
    supervisor: &Supervisor,
    route: &Route,
    failures: &mut Vec<String>,
    report: Report,
) -> bool {
    report(Progress::Connecting {
        route: route.to_string(),
    });
    let connected = connect(supervisor, route, report).and_then(|verified| {
        if !supervisor.publish(verified, true, Some(route)) {
            bail!("WARP changed before verification could be published");
        }
        Ok(())
    });
    match connected {
        Ok(()) => true,
        Err(error) => {
            report(Progress::AttemptFailed {
                route: route.to_string(),
                failure: Failure::from(&error),
            });
            failures.push(format!("{route}: {error:#}"));
            false
        }
    }
}

// Connects along `route`, migrates a relayed tunnel onto the direct path, and checks the result.
fn connect(supervisor: &Supervisor, route: &Route, report: Report) -> Result<Verified> {
    let (monitor, dataplane) = (&supervisor.monitor, &supervisor.dataplane);
    let proxy = supervisor.proxy();
    supervisor.private.run(|| {
        dataplane.block()?;
        warp::disconnect(monitor, &dataplane.observer)
    })?;
    // Authenticate on the host before WARP starts its short connection checks. Mudfish's
    // pacing wait must not consume the daemon's QUIC/Happy Eyeballs handshake deadline.
    dataplane.open(route.clone(), supervisor.plan.edge)?;
    supervisor
        .private
        .run(|| warp::connect(monitor, proxy, CONNECT, &dataplane.observer))?;
    if let Route::Relay(_) = route {
        report(Progress::Migrating);
        dataplane.migrate()?;
    }
    report(Progress::CheckingLocations);
    let verified = inspect_stable(supervisor, matches!(route, Route::Relay(_)))?;
    supervisor.plan.location.check(&verified.locations)?;
    Ok(verified)
}

fn inspect_stable(supervisor: &Supervisor, migrated: bool) -> Result<Verified> {
    let observer = &supervisor.dataplane.observer;
    let (locations, generation) = verify_generation(
        observer,
        CONNECT,
        QUIET,
        || {
            let (state, sequence) = supervisor.monitor.snapshot();
            state == State::Connected && sequence == observer.generation().control
        },
        |deadline, generation| {
            if !supervisor.reconcile_bridge()? {
                bail!("WARP's link is absent");
            }
            if observer.generation() != generation {
                bail!("WARP changed while restoring the bridge");
            }
            let locations = inspect(supervisor, deadline)?;
            // Opaque traffic still forwards normally. A migration, or an observed QUIC
            // session, needs a successful tunnel request rather than just CLI metadata.
            if (migrated || observer.generation().attempts > 0)
                && locations.probe4.is_none()
                && locations.probe6.is_none()
            {
                bail!("neither tunnel location probe succeeded");
            }
            Ok(locations)
        },
    )
    .context("verifying a stable WARP tunnel")?;
    Ok(Verified {
        locations,
        generation,
    })
}

fn verify_generation<T>(
    observer: &Observer,
    timeout: Duration,
    quiet: Duration,
    healthy: impl Fn() -> bool,
    inspect: impl Fn(Instant, Generation) -> Result<T>,
) -> Result<(T, Generation)> {
    let deadline = Instant::now() + timeout;
    let mut failure = anyhow::anyhow!("WARP is not healthy");
    loop {
        if observer.stopped() {
            bail!("the data plane stopped during tunnel verification");
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(failure.context("tunnel verification timed out"));
        }
        let generation = observer.generation();
        if healthy() {
            match inspect(deadline, generation) {
                Ok(value) => {
                    if observer.generation() != generation {
                        failure = anyhow::anyhow!("WARP changed while verifying the tunnel");
                        continue;
                    }
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if healthy()
                        && !remaining.is_zero()
                        && remaining >= quiet
                        && !observer.wait_change(generation, quiet)
                        && !observer.stopped()
                        && observer.generation() == generation
                        && healthy()
                    {
                        return Ok((value, generation));
                    }
                    failure = anyhow::anyhow!("WARP changed while verifying the tunnel");
                }
                Err(error) => failure = error,
            }
        }
        observer.wait_change(
            generation,
            RETRY.min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

// Reads every location field of the current tunnel from inside the namespace. Proxy instances
// probe through warp-svc's own proxy port; bridge instances through WARP's routes. Probe failures
// leave their fields empty.
fn inspect(supervisor: &Supervisor, deadline: Instant) -> Result<Locations> {
    let proxy = supervisor.proxy().then(warp::proxy_address);
    let (edge, probe4, probe6) = supervisor.private.run(|| {
        let observer = &supervisor.dataplane.observer;
        let edge = warp::tunnel(proxy.is_some(), observer)?;
        let probe = |family: Family| -> Option<Probe> {
            let timeout = PROBE.min(deadline.saturating_duration_since(Instant::now()));
            if observer.stopped() || timeout.is_zero() {
                return None;
            }
            probe::probe(family, proxy, timeout)
                .map_err(|error| warn!(?family, "location probe failed: {error:#}"))
                .ok()
        };
        std::thread::scope(|scope| {
            let probe4 = scope.spawn(|| probe(Family::V4));
            let probe6 = probe(Family::V6);
            let probe4 = probe4
                .join()
                .map_err(|_| anyhow::anyhow!("IPv4 location probe panicked"))?;
            Ok((edge, probe4, probe6))
        })
    })?;
    Ok(Locations::observe(
        edge,
        probe4,
        probe6,
        supervisor.geofeed.as_ref(),
    ))
}

// Refreshes the locations after a reconnect and rebootstraps only when a required field no longer
// matches.
pub(super) fn recheck(supervisor: &Supervisor) -> Result<()> {
    let verified = match inspect_stable(supervisor, false) {
        Ok(verified) => verified,
        Err(_) if supervisor.dataplane.observer.stopped() => return Ok(()),
        Err(error) => {
            warn!("reading locations failed; verification remains pending: {error:#}");
            return Ok(());
        }
    };
    let mismatch = supervisor.plan.location.check(&verified.locations).err();
    if !supervisor.publish(verified, mismatch.is_none(), None) {
        return Ok(());
    }
    let Some(error) = mismatch else {
        return Ok(());
    };
    warn!("location no longer matches: {error:#}");
    if !supervisor.plan.rebootstrap {
        return Ok(());
    }
    let started = Instant::now();
    bootstrap(supervisor, &|progress| {
        output::progress(
            supervisor.plan.instance.index,
            supervisor.plan.name.as_ref(),
            &progress,
        )
    })
    .context("rebootstrap failed")?;
    supervisor.observation.lock().unwrap().rebootstraps += 1;
    info!(elapsed = ?started.elapsed(), "rebootstrapped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reconnect_during_a_probe_repeats_verification() {
        use std::cell::Cell;

        let observer = Observer::default();
        observer.control_changed();
        let calls = Cell::new(0);
        let (value, generation) = verify_generation(
            &observer,
            Duration::from_secs(1),
            Duration::ZERO,
            || true,
            |_, _| {
                calls.set(calls.get() + 1);
                if calls.get() == 1 {
                    observer.control_changed();
                }
                Ok(calls.get())
            },
        )
        .unwrap();
        assert_eq!(value, 2);
        assert_eq!(generation.control, 2);
        assert!(observer.pending(Duration::ZERO).is_some());
        assert!(observer.confirm(generation));
    }

    #[test]
    fn failed_probes_leave_verification_pending() {
        let observer = Observer::default();
        observer.control_changed();
        let result = verify_generation::<()>(
            &observer,
            Duration::from_millis(10),
            Duration::ZERO,
            || true,
            |_, _| bail!("probe failed"),
        );
        assert!(result.is_err());
        assert!(observer.pending(Duration::ZERO).is_some());
    }

    #[test]
    fn successful_probes_do_not_override_daemon_health() {
        let observer = Observer::default();
        let healthy = std::cell::Cell::new(true);
        let result = verify_generation(
            &observer,
            Duration::from_millis(10),
            Duration::ZERO,
            || healthy.get(),
            |_, _| {
                healthy.set(false);
                Ok(())
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn shutdown_during_a_successful_probe_does_not_verify_it() {
        let observer = Observer::default();
        let result = verify_generation(
            &observer,
            Duration::from_secs(1),
            Duration::ZERO,
            || true,
            |_, _| {
                observer.stop();
                Ok(())
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn connection_failure_is_bounded_and_lists_recent_routes_on_separate_lines() {
        let failures = (0..5)
            .map(|index| format!("route {index}: refused"))
            .collect::<Vec<_>>();
        let message = connection_failure(&failures).to_string();
        assert!(message.starts_with("could not connect through any route (5 failed attempts)\nroute 2: refused\nroute 3: refused\nroute 4: refused\n"));
        assert!(!message.contains("route 0"));
        assert!(message.contains("2 earlier failures omitted"));
        assert!(message.contains("WAYWARP_LOG=debug"));
    }
}
