// The warp-svc process of one instance.
use crate::sandbox::Private;
use crate::tool;
use anyhow::{Context, Result, bail};
use nix::libc;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, info};

// Spawns warp-svc so that it dies with the spawning thread; `confined` also strips every
// capability, which keeps a host-network daemon from touching host routing or firewall state.
fn spawn(confined: bool) -> Result<Child> {
    let mut command = Command::new("warp-svc");
    tool::helper(&mut command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    if confined {
        // Only async-signal-safe prctl calls run between fork and exec.
        unsafe {
            command.pre_exec(|| {
                for capability in 0..=libc::c_ulong::from(u8::MAX) {
                    if libc::prctl(libc::PR_CAPBSET_DROP, capability) != 0 {
                        break;
                    }
                }
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    command.spawn().context("starting warp-svc")
}

const STOP: Duration = Duration::from_secs(3);
const READY: Duration = Duration::from_secs(20);

// Asks the daemon's process group to stop, escalating to SIGKILL once `STOP` passes.
fn terminate(group: Pid, mut exited: impl FnMut() -> bool) {
    let _ = killpg(group, Signal::SIGTERM);
    let started = Instant::now();
    while !exited() && started.elapsed() < STOP {
        thread::sleep(Duration::from_millis(50));
    }
    let _ = killpg(group, Signal::SIGKILL);
}

fn group(child: &Child) -> Pid {
    Pid::from_raw(child.id() as i32)
}

// warp-svc emits nothing when its IPC socket opens, so readiness is the one wait that polls: first
// for the socket file, then for a successful connection to it.
fn wait_ready(mut alive: impl FnMut() -> Result<()>) -> Result<()> {
    let socket = Path::new("/run/cloudflare-warp/warp_service");
    let started = Instant::now();
    while started.elapsed() < READY {
        alive()?;
        if socket.exists() && std::os::unix::net::UnixStream::connect(socket).is_ok() {
            debug!(elapsed = ?started.elapsed(), "warp-svc is ready");
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    bail!("warp-svc did not start within {} s", READY.as_secs())
}

// A warp-svc owned by a dedicated thread in the private namespace: the thread reaps it and
// reports its exit, and the parent-death signal kills it if the supervisor dies first.
pub struct Daemon {
    group: Pid,
    exited: Arc<AtomicBool>,
    owner: Option<thread::JoinHandle<Result<()>>>,
}

impl Daemon {
    pub fn start(private: &Private, on_exit: impl FnOnce(String) + Send + 'static) -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let exited = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&exited);
        let owner = private.spawn("warp-svc", move || {
            let mut child = match spawn(false) {
                Ok(child) => child,
                Err(error) => {
                    let _ = sender.send(Err(error));
                    return;
                }
            };
            let _ = sender.send(Ok(group(&child)));
            let status = child.wait();
            flag.store(true, Ordering::SeqCst);
            on_exit(match status {
                Ok(status) => status.to_string(),
                Err(error) => error.to_string(),
            });
        })?;
        let group = receiver.recv().context("warp-svc owner thread failed")??;
        info!(pid = group.as_raw(), "started warp-svc");
        let daemon = Self {
            group,
            exited,
            owner: Some(owner),
        };
        let started = wait_ready(|| {
            if daemon.exited.load(Ordering::SeqCst) {
                bail!("warp-svc exited during startup");
            }
            Ok(())
        });
        started.map(|()| daemon)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        terminate(self.group, || self.exited.load(Ordering::SeqCst));
        if let Some(owner) = self.owner.take() {
            let _ = owner.join();
        }
    }
}

pub fn registered() -> bool {
    Path::new("/var/lib/cloudflare-warp/reg.json").exists()
}

// Registration needs TCP, which the private namespace cannot reach, so it runs on the host network.
pub fn register(accept_tos: bool) -> Result<()> {
    super::require_consent(accept_tos)?;
    let mut daemon = spawn(true)?;
    let result = wait_ready(|| match daemon.try_wait()? {
        Some(status) => bail!("warp-svc exited during registration ({status})"),
        None => Ok(()),
    })
    .and_then(|()| super::cli(&["registration", "new"]));
    terminate(group(&daemon), || !matches!(daemon.try_wait(), Ok(None)));
    let _ = daemon.wait();
    result.context("registering a WARP device").map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_requires_consent_before_starting_a_daemon() {
        let error = register(false).unwrap_err().to_string();
        assert!(error.contains("--accept-tos"));
        assert!(error.contains("https://www.cloudflare.com/application/terms/"));
        assert!(crate::warp::require_consent(true).is_ok());
    }
}
