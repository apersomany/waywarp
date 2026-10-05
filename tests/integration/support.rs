use crate::test_support::TempDirectory;
use nix::sys::socket::{
    AddressFamily, Backlog, ControlMessageOwned, MsgFlags, SockFlag, SockType, UnixAddr, accept4,
    bind, listen, recvmsg, sendmsg, socket,
};
use nix::unistd::Uid;
use serde_json::{Value, json};
use std::fs;
use std::io::{IoSlice, IoSliceMut};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::process::Command;
use std::thread::{self, JoinHandle};

pub(super) fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_waywarp"));
    command.env("NO_COLOR", "1").env_remove("WAYWARP_LOG");
    command
}

pub(super) struct Fixture(TempDirectory);

impl Fixture {
    pub(super) fn new() -> Option<Self> {
        // Root uses fixed system paths; never inspect or modify the host's real instance store.
        if Uid::effective().is_root() {
            return None;
        }
        let root = TempDirectory::new("output");
        fs::create_dir_all(root.path.join("runtime/waywarp/2")).unwrap();
        Some(Self(root))
    }

    pub(super) fn path(&self) -> &Path {
        &self.0.path
    }

    pub(super) fn command(&self) -> Command {
        let mut command = command();
        command
            .env("HOME", &self.0.path)
            .env("XDG_STATE_HOME", self.0.path.join("state"))
            .env("XDG_RUNTIME_DIR", self.0.path.join("runtime"));
        command
    }

    pub(super) fn listener(&self) -> OwnedFd {
        let socket = socket(
            AddressFamily::Unix,
            SockType::SeqPacket,
            SockFlag::SOCK_CLOEXEC,
            None,
        )
        .unwrap();
        bind(
            socket.as_raw_fd(),
            &UnixAddr::new(&self.0.path.join("runtime/waywarp/2/control")).unwrap(),
        )
        .unwrap();
        listen(&socket, Backlog::new(1).unwrap()).unwrap();
        socket
    }

    pub(super) fn serve(
        &self,
        respond: impl FnOnce(Value, Vec<OwnedFd>) -> Option<Value> + Send + 'static,
    ) -> JoinHandle<()> {
        let socket = self.listener();
        thread::spawn(move || {
            // accept and SCM_RIGHTS each transfer a new descriptor to this thread.
            let client = unsafe {
                OwnedFd::from_raw_fd(accept4(socket.as_raw_fd(), SockFlag::SOCK_CLOEXEC).unwrap())
            };
            let mut bytes = [0; 4096];
            let mut ancillary = nix::cmsg_space!([i32; 2]);
            let mut slices = [IoSliceMut::new(&mut bytes)];
            let message = recvmsg::<()>(
                client.as_raw_fd(),
                &mut slices,
                Some(&mut ancillary),
                MsgFlags::MSG_CMSG_CLOEXEC,
            )
            .unwrap();
            let length = message.bytes;
            let mut descriptors = Vec::new();
            for control in message.cmsgs().unwrap() {
                if let ControlMessageOwned::ScmRights(rights) = control {
                    descriptors.extend(
                        rights
                            .into_iter()
                            .map(|raw| unsafe { OwnedFd::from_raw_fd(raw) }),
                    );
                }
            }
            let Some(response) = respond(
                serde_json::from_slice(&bytes[..length]).unwrap(),
                descriptors,
            ) else {
                return;
            };
            let response = serde_json::to_vec(&response).unwrap();
            sendmsg::<()>(
                client.as_raw_fd(),
                &[IoSlice::new(&response)],
                &[],
                MsgFlags::MSG_NOSIGNAL,
                None,
            )
            .unwrap();
        })
    }
}

pub(super) fn status(matched: bool) -> Value {
    json!({
        "index": 2, "name": "tokyo", "state": "connected", "matched": matched,
        "access": { "type": "proxy", "listen": "127.0.0.1:1082" },
        "locations": { "edge": "NRT", "geo4": null, "geo6": null, "probe4": null, "probe6": null },
        "relay": null, "rebootstraps": 0
    })
}
