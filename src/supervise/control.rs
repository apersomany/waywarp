// Serves `status`, `warp-cli`, and `down` on the instance's control socket.
use super::{Stop, Supervisor};
use crate::ipc::{self, Channel, Listener};
use crate::protocol::{Request, Response};
use crate::tool;
use anyhow::{Context, Result};
use std::os::fd::OwnedFd;
use std::process::Stdio;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;
use tracing::{debug, warn};

// Each client gets its own thread, so a slow warp-cli never delays `status`.
pub(super) fn serve(
    listener: Listener,
    supervisor: &Arc<Supervisor>,
    stop: &mpsc::Sender<Stop>,
) -> Result<()> {
    let (supervisor, stop) = (Arc::clone(supervisor), stop.clone());
    thread::Builder::new()
        .name("control".into())
        .spawn(move || {
            loop {
                let client = match listener.accept() {
                    Ok(client) => client,
                    Err(error) => {
                        warn!("control accept failed: {error:#}");
                        thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                };
                let (supervisor, stop) = (Arc::clone(&supervisor), stop.clone());
                let spawned =
                    thread::Builder::new()
                        .name("control-client".into())
                        .spawn(move || {
                            if let Err(error) = handle(&client, &supervisor, &stop) {
                                let _ = client.send(&Response::Failed(format!("{error:#}")), &[]);
                            }
                        });
                if let Err(error) = spawned {
                    warn!(%error, "cannot serve a control client");
                }
            }
        })?;
    Ok(())
}

fn handle(client: &Channel, supervisor: &Supervisor, stop: &mpsc::Sender<Stop>) -> Result<()> {
    client.set_timeout(Some(Duration::from_secs(10)))?;
    let (request, descriptors): (Request, Vec<OwnedFd>) = client.expect()?;
    client.set_timeout(None)?;
    debug!(?request, "control request");
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
            let _ = stop.send(Stop::Requested);
            // The client sees EOF when the process exits, which happens only after shutdown.
            loop {
                thread::park();
            }
        }
    }
}
