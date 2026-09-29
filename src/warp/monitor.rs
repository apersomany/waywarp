// Follows warp-svc's state through `warp-cli --listen status`.
use crate::sandbox::Private;
use crate::tool;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader};
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
}

// Follows `warp-cli --listen status`, so waits react to transitions instead of polling the daemon.
#[derive(Clone)]
pub struct Monitor(Arc<(Mutex<Shared>, Condvar)>);

impl Monitor {
    // Listens inside the private namespace and restarts the listener whenever it exits until `stop`.
    pub fn start(private: &Private) -> Result<Self> {
        let monitor = Self(Arc::new((
            Mutex::new(Shared {
                state: State::Unknown,
                sequence: 0,
                stopped: false,
            }),
            Condvar::new(),
        )));
        let listener = monitor.clone();
        private.spawn("warp-listen", move || {
            while !listener.stopped() {
                if let Err(error) = listener.follow() {
                    warn!("warp-cli status listener failed: {error:#}");
                }
                thread::sleep(Duration::from_millis(500));
            }
        })?;
        Ok(monitor)
    }

    fn follow(&self) -> Result<()> {
        let mut child = tool::helper(
            Command::new("warp-cli")
                .args(["--accept-tos", "--json", "--listen", "status"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null()),
        )
        .spawn()
        .context("starting warp-cli --listen")?;
        let stdout = child.stdout.take().context("warp-cli stdout")?;
        for line in BufReader::new(stdout).lines() {
            if self.stopped() {
                break;
            }
            let line = line?;
            if let Some(state) = State::parse(&line) {
                if state == State::Unknown {
                    debug!(line, "unrecognized WARP status");
                }
                info!(%state, "WARP state changed");
                self.publish(state);
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        Ok(())
    }

    // A panicking holder cannot leave `Shared` inconsistent: every update is a single assignment.
    fn shared(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.0.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn stopped(&self) -> bool {
        self.shared().stopped
    }

    fn publish(&self, state: State) {
        let (shared, changed) = &*self.0;
        let mut shared = shared.lock().unwrap_or_else(PoisonError::into_inner);
        shared.state = state;
        shared.sequence += 1;
        changed.notify_all();
    }

    pub fn state(&self) -> State {
        self.shared().state
    }

    pub fn sequence(&self) -> u64 {
        self.shared().sequence
    }

    // Blocks until an event after `since` satisfies `done`, returning its sequence number.
    pub fn wait(
        &self,
        since: u64,
        timeout: Duration,
        done: impl Fn(&State) -> bool,
    ) -> Option<u64> {
        let (shared, changed) = &*self.0;
        let deadline = Instant::now() + timeout;
        let mut shared = shared.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if shared.sequence > since && done(&shared.state) {
                return Some(shared.sequence);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || shared.stopped {
                return None;
            }
            shared = changed
                .wait_timeout(shared, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    pub fn stop(&self) {
        let (shared, changed) = &*self.0;
        shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .stopped = true;
        changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
