use super::quic::Change;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tracing::debug;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Generation {
    pub attempts: u64,
    pub exchanges: u64,
    pub path: u64,
    pub control: u64,
    pub links: u64,
}

#[derive(Default)]
struct Shared {
    generation: Generation,
    verified: Generation,
    stopped: bool,
}

// Generations are durable work, not a queue of transient states. Verification can acknowledge
// only its starting generation; changes during a probe remain pending for the next check.
#[derive(Clone, Default)]
pub struct Observer(Arc<(Mutex<Shared>, Condvar)>);

impl Observer {
    fn shared(&self) -> MutexGuard<'_, Shared> {
        self.0.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn generation(&self) -> Generation {
        self.shared().generation
    }

    pub(super) fn observe(&self, change: Change) {
        if change.attempts == 0 && change.exchanges == 0 {
            return;
        }
        let mut shared = self.shared();
        shared.generation.attempts += change.attempts;
        shared.generation.exchanges += change.exchanges;
        debug!(
            attempts = shared.generation.attempts,
            exchanges = shared.generation.exchanges,
            "observed WARP QUIC activity"
        );
        self.0.1.notify_all();
    }

    pub fn control_changed(&self) {
        self.shared().generation.control += 1;
        self.0.1.notify_all();
    }

    pub fn link_changed(&self) {
        self.shared().generation.links += 1;
        self.0.1.notify_all();
    }

    pub(super) fn path_changed(&self) {
        self.shared().generation.path += 1;
        self.0.1.notify_all();
    }

    pub fn confirm(&self, generation: Generation) -> bool {
        let mut shared = self.shared();
        if shared.stopped || shared.generation != generation {
            return false;
        }
        shared.verified = generation;
        true
    }

    pub fn pending(&self, timeout: Duration) -> Option<Generation> {
        self.wait(timeout, |shared| shared.generation != shared.verified)
    }

    pub fn wait_change(&self, since: Generation, timeout: Duration) -> bool {
        self.wait(timeout, |shared| shared.generation != since)
            .is_some()
    }

    fn wait(&self, timeout: Duration, done: impl Fn(&Shared) -> bool) -> Option<Generation> {
        let deadline = Instant::now() + timeout;
        let mut shared = self.shared();
        loop {
            if shared.stopped {
                return None;
            }
            if done(&shared) {
                return Some(shared.generation);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            shared = self
                .0
                .1
                .wait_timeout(shared, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    pub fn stopped(&self) -> bool {
        self.shared().stopped
    }

    pub fn wait_stopped(&self, timeout: Duration) -> bool {
        self.wait(timeout, |_| false);
        self.stopped()
    }

    pub fn stop(&self) {
        self.shared().stopped = true;
        self.0.1.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn reconnects_during_verification_cannot_be_acknowledged_by_old_probes() {
        let observer = Observer::default();
        observer.observe(Change {
            attempts: 1,
            ..Change::default()
        });
        let verifying = observer.generation();
        observer.observe(Change {
            attempts: 1,
            ..Change::default()
        });
        assert!(!observer.confirm(verifying));
        assert_eq!(observer.pending(Duration::ZERO).unwrap().attempts, 2);
        assert!(observer.confirm(observer.generation()));
        assert!(observer.pending(Duration::ZERO).is_none());
    }

    #[test]
    fn shutdown_wakes_blocked_verifiers() {
        let observer = Observer::default();
        let waiter = observer.clone();
        let thread = thread::spawn(move || waiter.pending(Duration::from_secs(60)));
        observer.stop();
        assert!(thread.join().unwrap().is_none());
        assert!(!observer.confirm(observer.generation()));
    }
}
