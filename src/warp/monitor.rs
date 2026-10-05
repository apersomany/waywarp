// Follows warp-svc's state through `warp-cli --listen status`.
use crate::sandbox::Private;
use crate::tool;
use anyhow::{Context, Result};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Unknown,
    Disconnected,
    // The last attempt gave up, as with HappyEyeballsFailed; the daemon is idle until told otherwise.
    Unable,
    Connecting,
    Degraded,
    Connected,
}

impl State {
    fn parse(line: &str) -> Option<Self> {
        #[derive(Deserialize)]
        struct Event {
            status: Option<String>,
            reason: Option<serde_json::Value>,
        }
        let event: Event = serde_json::from_str(line).ok()?;
        let reason = |name: &str| match &event.reason {
            Some(serde_json::Value::String(reason)) => reason == name,
            Some(serde_json::Value::Object(reason)) => reason.contains_key(name),
            _ => false,
        };
        Some(match event.status?.as_str() {
            "Connected" if reason("NetworkHealthy") => Self::Connected,
            "Connected" => Self::Degraded,
            "Connecting" => Self::Connecting,
            "Disconnected" => Self::Disconnected,
            "Unable" => Self::Unable,
            _ => Self::Unknown,
        })
    }
}

impl std::fmt::Display for State {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Unknown => "unknown",
            Self::Disconnected => "disconnected",
            Self::Unable => "unable to connect",
            Self::Connecting => "connecting",
            Self::Degraded => "degraded",
            Self::Connected => "connected",
        })
    }
}

struct Shared {
    state: State,
    // Increments on every event so waiters can tell a fresh transition from a stale one.
    sequence: u64,
    stopped: bool,
    process: Option<Pid>,
}

// Follows `warp-cli --listen status`, so waits react to transitions instead of polling the daemon.
#[derive(Clone)]
pub struct Monitor {
    shared: Arc<(Mutex<Shared>, Condvar)>,
    changed: Arc<dyn Fn() + Send + Sync>,
    owner: Arc<Mutex<Option<thread::JoinHandle<Result<()>>>>>,
}

impl Monitor {
    pub fn new(changed: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            shared: Arc::new((
                Mutex::new(Shared {
                    state: State::Unknown,
                    sequence: 0,
                    stopped: false,
                    process: None,
                }),
                Condvar::new(),
            )),
            changed: Arc::new(changed),
            owner: Arc::new(Mutex::new(None)),
        }
    }

    // Listens inside the private namespace and restarts the listener whenever it exits until `stop`.
    pub fn start(&self, private: &Private) -> Result<()> {
        let mut owner = self.owner.lock().unwrap_or_else(PoisonError::into_inner);
        let listener = self.clone();
        *owner = Some(private.spawn("warp-listen", move || {
            while !listener.stopped() {
                if let Err(error) = listener.follow() {
                    warn!("warp-cli status listener failed: {error:#}");
                }
                if !listener.stopped() {
                    listener.publish(State::Unknown);
                    thread::sleep(Duration::from_millis(500));
                }
            }
        })?);
        drop(owner);
        if self.stopped() {
            self.stop();
        }
        Ok(())
    }

    fn follow(&self) -> Result<()> {
        let mut child = tool::helper(
            Command::new("warp-cli")
                .args(["--accept-tos", "--json", "--listen", "status"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .process_group(0),
        )
        .spawn()
        .context("starting warp-cli --listen")?;
        let stdout = child.stdout.take().context("warp-cli stdout")?;
        let group = Pid::from_raw(child.id() as i32);
        {
            let mut shared = self.shared();
            if shared.stopped {
                let _ = killpg(group, Signal::SIGKILL);
                let _ = child.wait();
                return Ok(());
            }
            shared.process = Some(group);
        }
        let followed = (|| -> Result<()> {
            for line in BufReader::new(stdout).lines() {
                if self.stopped() {
                    break;
                }
                let line = line.context("reading warp-cli status")?;
                if let Some(state) = State::parse(&line) {
                    if state == State::Unknown {
                        debug!(line, "unrecognized WARP status");
                    }
                    info!(%state, "WARP state changed");
                    self.publish(state);
                }
            }
            Ok(())
        })();
        let _ = killpg(group, Signal::SIGKILL);
        self.shared().process = None;
        let _ = child.wait();
        followed
    }
    fn shared(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn stopped(&self) -> bool {
        self.shared().stopped
    }

    fn publish(&self, state: State) {
        let (shared, changed) = &*self.shared;
        let mut shared = shared.lock().unwrap_or_else(PoisonError::into_inner);
        if shared.stopped {
            return;
        }
        shared.state = state;
        shared.sequence += 1;
        changed.notify_all();
        drop(shared);
        (self.changed)();
    }

    pub fn snapshot(&self) -> (State, u64) {
        let shared = self.shared();
        (shared.state, shared.sequence)
    }

    pub fn state(&self) -> State {
        self.snapshot().0
    }

    pub fn sequence(&self) -> u64 {
        self.snapshot().1
    }

    // Blocks until an event after `since` satisfies `done`, returning its sequence number.
    pub fn wait(
        &self,
        since: u64,
        timeout: Duration,
        done: impl Fn(&State) -> bool,
    ) -> Option<u64> {
        let (shared, changed) = &*self.shared;
        let deadline = Instant::now() + timeout;
        let mut shared = shared.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if shared.stopped {
                return None;
            }
            if shared.sequence > since && done(&shared.state) {
                return Some(shared.sequence);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            shared = changed
                .wait_timeout(shared, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    pub fn cancel(&self) {
        let (shared, changed) = &*self.shared;
        let mut shared = shared.lock().unwrap_or_else(PoisonError::into_inner);
        shared.stopped = true;
        if let Some(group) = shared.process.take() {
            let _ = killpg(group, Signal::SIGKILL);
        }
        changed.notify_all();
    }

    pub fn stop(&self) {
        self.cancel();
        let owner = self
            .owner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(owner) = owner {
            let _ = owner.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_reconnects_notify_verification_even_after_the_state_is_healthy() {
        let observer = crate::dataplane::Observer::default();
        let changes = observer.clone();
        let monitor = Monitor::new(move || changes.control_changed());
        monitor.publish(State::Connected);
        assert!(observer.confirm(observer.generation()));
        monitor.publish(State::Connecting);
        monitor.publish(State::Connected);
        assert_eq!(monitor.state(), State::Connected);
        assert!(observer.pending(Duration::ZERO).is_some());
    }

    #[test]
    fn readiness_never_accepts_a_historical_connected_event() {
        let monitor = Monitor::new(|| {});
        monitor.publish(State::Connected);
        monitor.publish(State::Disconnected);
        assert!(
            monitor
                .wait(0, Duration::ZERO, |state| *state == State::Connected)
                .is_none()
        );
    }

    #[test]
    fn shutdown_rejects_readiness_and_wakes_waiters() {
        let monitor = Monitor::new(|| {});
        monitor.publish(State::Connected);
        let waiter = monitor.clone();
        let thread = thread::spawn(move || waiter.wait(1, Duration::from_secs(60), |_| true));
        monitor.stop();
        assert!(thread.join().unwrap().is_none());
        assert!(monitor.wait(0, Duration::ZERO, |_| true).is_none());
        monitor.publish(State::Disconnected);
        assert_eq!(monitor.sequence(), 1);
    }

    #[test]
    fn only_a_healthy_connection_counts_as_connected() {
        let parse = |line: &str| State::parse(line);
        assert_eq!(
            parse(r#"{"status":"Connected","reason":"NetworkHealthy"}"#),
            Some(State::Connected)
        );
        assert_eq!(
            parse(r#"{"status":"Connected","reason":{"NetworkDegraded":{"rtt_ms":6}}}"#),
            Some(State::Degraded)
        );
        assert_eq!(
            parse(r#"{"status":"Unable","reason":"HappyEyeballsFailed"}"#),
            Some(State::Unable)
        );
        assert_eq!(parse(r#"{"update":"SettingsUpdated"}"#), None);
    }
}
