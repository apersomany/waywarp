// Tells systemd a Type=notify service is ready (sd_notify(3)).
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};

pub struct Notifier(Option<SocketAddr>);

impl Notifier {
    // Takes NOTIFY_SOCKET out of the environment so helper processes cannot claim readiness.
    // Must run while the process is single-threaded.
    pub fn take() -> Self {
        let path = std::env::var_os("NOTIFY_SOCKET");
        // Sound because the caller guarantees no other thread reads the environment.
        unsafe { std::env::remove_var("NOTIFY_SOCKET") };
        let address = path.and_then(|path| {
            let bytes = path.as_encoded_bytes();
            match bytes.strip_prefix(b"@") {
                Some(name) => SocketAddr::from_abstract_name(name).ok(),
                None => SocketAddr::from_pathname(&path).ok(),
            }
        });
        Self(address)
    }

    pub fn ready(&self, status: &str) {
        let Some(address) = &self.0 else {
            return;
        };
        let message = format!("READY=1\nSTATUS={}", status.replace('\n', " "));
        let sent = UnixDatagram::unbound()
            .and_then(|socket| socket.send_to_addr(message.as_bytes(), address));
        if let Err(error) = sent {
            tracing::warn!(%error, "cannot notify systemd");
        }
    }
}
