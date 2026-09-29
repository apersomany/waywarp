// Connects WARP through each --via entry in order until the tunnel satisfies --location.
use super::{Report, Supervisor};
use crate::location::Locations;
use crate::location::probe::{self, Family, Probe};
use crate::via::{self, Route, ping};
use crate::warp::{self, State};
use anyhow::{Context, Result, bail};
use std::time::{Duration, Instant};
use tracing::{info, warn};

const CONNECT: Duration = Duration::from_secs(45);
const MIGRATE: Duration = Duration::from_secs(20);
// Reconnects within this long after a migration are part of settling, not drift.
const SETTLE: Duration = Duration::from_secs(3);

// Consecutive relays are pinged four at a time; the colo each ping reports only orders the
// attempts, since the hint can be wrong.
pub(super) fn bootstrap(supervisor: &Supervisor, report: Report) -> Result<()> {
    let plan = &supervisor.plan;
    let routes = via::expand(&plan.via, &plan.credentials, plan.mudfish_port)?;
    let mut failures = Vec::new();
    let mut routes = routes.into_iter().peekable();
    while let Some(route) = routes.next() {
        let Route::Relay(relay) = route else {
            if let Some(locations) = attempt(supervisor, &route, &mut failures, report) {
                return finish(supervisor, locations, &route);
            }
            continue;
        };
        let mut relays = vec![relay];
        while let Some(Route::Relay(relay)) =
            routes.next_if(|route| matches!(route, Route::Relay(_)))
        {
            relays.push(relay);
        }
        let mut deferred = Vec::new();
        for (relay, ping) in ping::stream(&plan.interface, relays, plan.edge) {
            let ping = match ping {
                Ok(ping) => ping,
                Err(error) => {
                    failures.push(format!("via {}: {error:#}", relay.label));
                    continue;
                }
            };
            let hint = ping
                .colo
                .as_deref()
                .map(|colo| format!(", likely {colo}"))
                .unwrap_or_default();
            report(format!(
                "pinged {} ({} ms{hint})",
                relay.label,
                ping.rtt.as_millis()
            ));
            let plausible = ping
                .colo
                .as_deref()
                .is_none_or(|colo| plan.location.plausible(colo, supervisor.geofeed.as_ref()));
            if !plausible {
                deferred.push((ping.rtt, relay));
                continue;
            }
            let route = Route::Relay(relay);
            if let Some(locations) = attempt(supervisor, &route, &mut failures, report) {
                return finish(supervisor, locations, &route);
            }
        }
        deferred.sort_by_key(|(rtt, _)| *rtt);
        for (_, relay) in deferred {
            let route = Route::Relay(relay);
            if let Some(locations) = attempt(supervisor, &route, &mut failures, report) {
                return finish(supervisor, locations, &route);
            }
        }
    }
    let omitted = failures.len().saturating_sub(3);
    let prefix = match omitted {
        0 => String::new(),
        count => format!("{count} earlier failures omitted; "),
    };
    bail!(
        "could not connect ({prefix}{})",
        failures[omitted..].join("; ")
    )
}

fn finish(supervisor: &Supervisor, locations: Locations, route: &Route) -> Result<()> {
    let mut status = supervisor.status.lock().unwrap();
    status.locations = locations;
    status.relay = match route {
        Route::Direct => None,
        Route::Relay(relay) => Some(relay.label.clone()),
    };
    status.matched = true;
    Ok(())
}

fn attempt(
    supervisor: &Supervisor,
    route: &Route,
    failures: &mut Vec<String>,
    report: Report,
) -> Option<Locations> {
    report(format!("connecting {route}"));
    match connect(supervisor, route) {
        Ok(locations) => Some(locations),
        Err(error) => {
            report(format!("connecting {route} failed: {error:#}"));
            failures.push(format!("{route}: {error:#}"));
            None
        }
    }
}

// Connects along `route`, migrates a relayed tunnel onto the direct path, and checks the result.
fn connect(supervisor: &Supervisor, route: &Route) -> Result<Locations> {
    let (monitor, dataplane) = (&supervisor.monitor, &supervisor.dataplane);
    let proxy = supervisor.proxy();
    supervisor.private.run(|| {
        dataplane.block();
        warp::disconnect(monitor)?;
        dataplane.open(route.clone());
        warp::connect(monitor, proxy, CONNECT)
    })?;
    if let Route::Relay(_) = route {
        migrate(supervisor)?;
    }
    let locations = inspect(supervisor)?;
    supervisor.plan.location.check(&locations)?;
    Ok(locations)
}

fn migrate(supervisor: &Supervisor) -> Result<()> {
    let monitor = &supervisor.monitor;
    let since = monitor.sequence();
    supervisor.dataplane.migrate(MIGRATE)?;
    // warp-svc may reconnect instead of migrating, notably once after startup when it finishes
    // discovering the network. Reconnects reuse the colo encoded in the connection ID.
    let reconnecting = |state: &State| matches!(state, State::Disconnected | State::Connecting);
    if let Some(sequence) = monitor.wait(since, SETTLE, reconnecting) {
        info!("WARP reconnected on the direct path");
        if monitor
            .wait(sequence, CONNECT, |state| *state == State::Connected)
            .is_none()
        {
            bail!(
                "WARP did not reconnect on the direct path (last state: {})",
                monitor.state()
            );
        }
    }
    Ok(())
}

// Reads every location field of the current tunnel from inside the namespace. Proxy instances
// probe through warp-svc's own proxy port; bridge instances through WARP's routes. Probe failures
// leave their fields empty.
fn inspect(supervisor: &Supervisor) -> Result<Locations> {
    let proxy = supervisor.proxy().then(warp::proxy_address);
    let (edge, probe4, probe6) = supervisor.private.run(|| {
        let edge = warp::tunnel(proxy.is_some())?;
        let probe = |family: Family| -> Option<Probe> {
            probe::probe(family, proxy)
                .map_err(|error| warn!(?family, "location probe failed: {error:#}"))
                .ok()
        };
        Ok((edge, probe(Family::V4), probe(Family::V6)))
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
    let locations = match inspect(supervisor) {
        Ok(locations) => locations,
        Err(error) => {
            warn!("reading locations failed: {error:#}");
            return Ok(());
        }
    };
    let mismatch = supervisor.plan.location.check(&locations).err();
    {
        let mut status = supervisor.status.lock().unwrap();
        status.locations = locations;
        status.matched = mismatch.is_none();
    }
    let Some(error) = mismatch else {
        return Ok(());
    };
    warn!("location no longer matches: {error:#}");
    if !supervisor.plan.rebootstrap {
        return Ok(());
    }
    let started = Instant::now();
    bootstrap(supervisor, &|message| info!("{message}")).context("rebootstrap failed")?;
    supervisor.status.lock().unwrap().rebootstraps += 1;
    info!(elapsed = ?started.elapsed(), "rebootstrapped");
    Ok(())
}
