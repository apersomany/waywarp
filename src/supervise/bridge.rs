use super::{Stop, Supervisor};
use crate::bridge::{self, Attachment, WarpLink, nat};
use crate::protocol::{Access, Plan};
use crate::sandbox::Private;
use crate::warp::State;
use anyhow::{Context, Result};
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{info, warn};

pub(super) struct Bridge {
    attachment: Mutex<Option<Attachment>>,
    private: Private,
    registration: PathBuf,
    subnets: bridge::Subnets,
    mode: nat::Mode,
    policy: Mutex<Option<nat::Policy>>,
}

impl Bridge {
    pub fn attach(plan: &Plan, private: &Private) -> Result<Option<Self>> {
        let Access::Bridge {
            subnets, nat: mode, ..
        } = plan.access
        else {
            return Ok(None);
        };
        let attachment = bridge::attach(plan.instance.index, subnets, private.path())?;
        private.run(|| bridge::firewall(subnets))?;
        Ok(Some(Self {
            attachment: Mutex::new(Some(attachment)),
            private: private.clone(),
            registration: plan.instance.registration(),
            subnets,
            mode,
            policy: Mutex::new(None),
        }))
    }

    pub fn policy(&self) -> Option<nat::Policy> {
        self.policy.lock().unwrap().clone()
    }

    // Reconnects and file events reconcile the same desired state under one lock.
    pub fn reconcile(&self) -> Result<()> {
        let mut current = self.policy.lock().unwrap();
        let link = self.private.run(WarpLink::read)?;
        let contents = std::fs::read(self.registration.join("conf.json")).ok();
        let mut next = nat::Policy::read(self.mode, contents.as_deref(), current.as_ref());
        next.verify_addresses(&link.addresses);
        self.private.run(|| link.reconcile())?;
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
        Ok(())
    }

    pub fn stop(&self) {
        self.attachment.lock().unwrap().take();
    }
}

// Watch the directory so atomic replacement of conf.json cannot detach the watch.
pub(super) fn watch(supervisor: &Arc<Supervisor>, stop: &mpsc::Sender<Stop>) -> Result<()> {
    let Some(bridge) = &supervisor.bridge else {
        return Ok(());
    };
    let watcher = Inotify::init(InitFlags::IN_CLOEXEC | InitFlags::IN_NONBLOCK)?;
    watcher.add_watch(
        &bridge.registration,
        AddWatchFlags::IN_CLOSE_WRITE
            | AddWatchFlags::IN_MOVED_TO
            | AddWatchFlags::IN_DELETE
            | AddWatchFlags::IN_CREATE,
    )?;
    // Close the gap between bootstrap and installing the watch.
    bridge.reconcile()?;
    let (supervisor, failed) = (Arc::clone(supervisor), stop.clone());
    thread::Builder::new()
        .name("bridge".into())
        .spawn(move || {
            let mut last = Instant::now();
            while !supervisor.monitor.stopped() {
                let changed = match watcher.read_events() {
                    Ok(events) => events.iter().any(|event| {
                        event.mask.contains(AddWatchFlags::IN_Q_OVERFLOW)
                            || event.name.as_deref() == Some(std::ffi::OsStr::new("conf.json"))
                    }),
                    Err(nix::errno::Errno::EAGAIN) => false,
                    Err(error) => {
                        let _ = failed.send(Stop::Failed(format!(
                            "watching bridge configuration: {error}"
                        )));
                        return;
                    }
                };
                if changed || last.elapsed() >= Duration::from_secs(5) {
                    if let Err(error) = supervisor.reconcile_bridge() {
                        // Link absence is normal during reconnect; the connection hook retries.
                        if supervisor.monitor.state() == State::Connected {
                            let _ =
                                failed.send(Stop::Failed(format!("reconciling bridge: {error:#}")));
                            return;
                        }
                    }
                    last = Instant::now();
                }
                thread::sleep(Duration::from_millis(250));
            }
        })?;
    Ok(())
}
