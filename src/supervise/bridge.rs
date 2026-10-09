use super::{Stop, Stopper, Supervisor};
use crate::bridge::{self, Attachment, Routing, WarpLink, nat, watch::LinkEvents};
use crate::dataplane::Observer;
use crate::protocol::{Access, Plan};
use crate::sandbox::Private;
use anyhow::{Context, Result};
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tracing::{info, warn};

pub(super) struct Bridge {
    attachment: Mutex<Option<Attachment>>,
    private: Private,
    registration: PathBuf,
    subnets: bridge::Subnets,
    routing: Routing,
    mode: nat::Mode,
    policy: Mutex<Option<nat::Policy>>,
    links: Mutex<LinkEvents>,
    observer: Observer,
}

impl Bridge {
    pub fn attach(plan: &Plan, private: &Private, observer: &Observer) -> Result<Option<Self>> {
        let Access::Bridge {
            subnets, nat: mode, ..
        } = plan.access
        else {
            return Ok(None);
        };
        let attachment = bridge::attach(plan.instance.index, subnets, private.path())?;
        let routing = private.run(|| bridge::firewall(subnets))?;
        Ok(Some(Self {
            attachment: Mutex::new(Some(attachment)),
            private: private.clone(),
            registration: plan.instance.registration(),
            subnets,
            routing,
            mode,
            policy: Mutex::new(None),
            links: Mutex::new(private.run(LinkEvents::open)?),
            observer: observer.clone(),
        }))
    }

    pub fn policy(&self) -> Option<nat::Policy> {
        self.policy.lock().unwrap().clone()
    }

    fn links_changed(&self) -> Result<bool> {
        let changed = self.links.lock().unwrap().changed()?;
        if changed {
            self.observer.link_changed();
        }
        Ok(changed)
    }

    // Reconnects and file events reconcile the same desired state under one lock.
    pub fn reconcile(&self) -> Result<bool> {
        self.links_changed()?;
        let mut current = self.policy.lock().unwrap();
        let Some(link) = self.private.run(WarpLink::read)? else {
            return Ok(false);
        };
        let contents = std::fs::read(self.registration.join("conf.json")).ok();
        let mut next = nat::Policy::read(self.mode, contents.as_deref(), current.as_ref());
        next.verify_addresses(&link.addresses);
        self.private.run(|| link.reconcile(&self.routing))?;
        self.attachment
            .lock()
            .unwrap()
            .as_ref()
            .context("bridge stopped")?
            .set_mtu(link.mtu)?;
        if current.as_ref() != Some(&next) {
            self.private.run(|| next.apply(self.subnets))?;
            if !next.configuration_valid {
                warn!("bridge configuration unavailable or invalid; removing routed exemptions");
            }
            info!(mode = ?next.mode, v4 = ?next.v4, v6 = ?next.v6, routed = ?next.routed,
                "updated bridge NAT (existing connections keep their NAT mapping)");
            *current = Some(next);
        }
        Ok(true)
    }

    pub fn stop(&self) {
        self.attachment.lock().unwrap().take();
    }
}

// Watch the directory so atomic replacement of conf.json cannot detach the watch.
pub(super) fn watch(
    supervisor: &Arc<Supervisor>,
    stop: &Stopper,
) -> Result<Option<JoinHandle<()>>> {
    let Some(bridge) = &supervisor.bridge else {
        return Ok(None);
    };
    let watcher = Inotify::init(InitFlags::IN_CLOEXEC | InitFlags::IN_NONBLOCK)?;
    watcher.add_watch(
        &bridge.registration,
        AddWatchFlags::IN_CLOSE_WRITE
            | AddWatchFlags::IN_MOVED_TO
            | AddWatchFlags::IN_DELETE
            | AddWatchFlags::IN_CREATE,
    )?;
    let (supervisor, failed) = (Arc::clone(supervisor), stop.clone());
    let owner = thread::Builder::new()
        .name("bridge".into())
        .spawn(move || {
            let bridge = supervisor.bridge.as_ref().unwrap();
            let mut last = Instant::now();
            // Re-snapshot after installing the file watch, with the same retry policy used for
            // subsequent events if WARP recreates its link while the watcher starts.
            let mut initial = true;
            let mut failed_since = None;
            while !supervisor.dataplane.observer.stopped() {
                let changed = match watcher.read_events() {
                    Ok(events) => events.iter().any(|event| {
                        event.mask.contains(AddWatchFlags::IN_Q_OVERFLOW)
                            || event.name.as_deref() == Some(std::ffi::OsStr::new("conf.json"))
                    }),
                    Err(nix::errno::Errno::EAGAIN) => false,
                    Err(error) => {
                        failed.request(Stop::Failed(
                            anyhow::Error::new(error).context("watching bridge configuration"),
                        ));
                        return;
                    }
                };
                let links_changed = match bridge.links_changed() {
                    Ok(changed) => changed,
                    Err(error) => {
                        failed.request(Stop::Failed(error));
                        return;
                    }
                };
                if initial
                    || changed
                    || links_changed
                    || failed_since.is_some()
                    || last.elapsed() >= Duration::from_secs(5)
                {
                    match bridge.reconcile() {
                        Ok(_) => failed_since = None,
                        Err(error) => {
                            // The link can disappear between its snapshot and route updates.
                            // Retry that race independently of possibly stale CLI health.
                            let since = failed_since.get_or_insert_with(Instant::now);
                            if since.elapsed() >= Duration::from_secs(5) {
                                failed.request(Stop::Failed(error.context("reconciling bridge")));
                                return;
                            }
                            warn!("bridge reconciliation will retry: {error:#}");
                        }
                    }
                    initial = false;
                    last = Instant::now();
                }
                thread::sleep(Duration::from_millis(250));
            }
        })?;
    Ok(Some(owner))
}
