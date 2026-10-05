// Serves `status`, `warp-cli`, and `down` on the instance's control socket.
use super::{Stop, Stopper, Supervisor};
use crate::ipc::{self, Channel, Listener};
use crate::protocol::{Failure, Request, Response};
use crate::tool;
use anyhow::{Context, Result};
use std::os::fd::OwnedFd;
use std::process::Stdio;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tracing::warn;

// Each client gets its own thread, so a slow warp-cli never delays `status`.
pub(super) fn serve(
    listener: Listener,
    supervisor: &Arc<Supervisor>,
    stop: &Stopper,
) -> Result<JoinHandle<()>> {
    listener.set_nonblocking()?;
    let (supervisor, stop) = (Arc::clone(supervisor), stop.clone());
    Ok(thread::Builder::new()
        .name("control".into())
        .spawn(move || {
            while !stop.observer.stopped() {
                let client = match listener.accept() {
                    Ok(client) => client,
                    Err(error)
                        if error.downcast_ref::<nix::errno::Errno>()
                            == Some(&nix::errno::Errno::EAGAIN) =>
                    {
                        thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                    Err(error)
                        if error.downcast_ref::<nix::errno::Errno>()
                            == Some(&nix::errno::Errno::EINTR) =>
                    {
                        continue;
                    }
                    Err(error) => {
                        stop.request(Stop::Failed(error.context("accepting a control client")));
                        return;
                    }
                };
                let (supervisor, stop) = (Arc::clone(&supervisor), stop.clone());
                let spawned =
                    thread::Builder::new()
                        .name("control-client".into())
                        .spawn(move || {
                            if let Err(error) = handle(&client, &supervisor, &stop) {
                                let _ = client.send(&Response::Failed(Failure::from(&error)), &[]);
                            }
                        });
                if let Err(error) = spawned {
                    warn!(%error, "cannot serve a control client");
                }
            }
        })?)
}

fn handle(client: &Channel, supervisor: &Supervisor, stop: &Stopper) -> Result<()> {
    client.set_timeout(Some(Duration::from_secs(10)))?;
    let (request, descriptors): (Request, Vec<OwnedFd>) = client.expect()?;
    client.set_timeout(None)?;
    match request {
        Request::Status => client.send(&Response::Status(Box::new(supervisor.status())), &[]),
        Request::WarpCli(arguments) => {
            let [stdout, stderr] = ipc::exact::<2>(descriptors)?;
            let code = supervisor.private.run(|| {
                let status = tool::helper(
                    std::process::Command::new("warp-cli")
                        .args(arguments)
                        .stdin(Stdio::null())
                        .stdout(stdout)
                        .stderr(stderr),
                )
                .status()
                .context("running warp-cli")?;
                Ok(status.code().unwrap_or(1))
            })?;
            client.send(&Response::Exit(code), &[])
        }
        Request::Stop => {
            stop.request(Stop::Requested(Some(client.try_clone()?)));
            Ok(())
        }
    }
}
