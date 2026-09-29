use anyhow::{Context, Result, bail};
use nix::sys::socket::{
    AddressFamily, Backlog, ControlMessage, ControlMessageOwned, MsgFlags, SockFlag, SockType,
    UnixAddr, accept4, bind, connect, listen, recvmsg, sendmsg, socket, socketpair,
};
use serde::{Serialize, de::DeserializeOwned};
use std::io::{IoSlice, IoSliceMut};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

const MAX_MESSAGE: usize = 64 * 1024;
const MAX_DESCRIPTORS: usize = 8;

fn seqpacket() -> Result<OwnedFd> {
    Ok(socket(
        AddressFamily::Unix,
        SockType::SeqPacket,
        SockFlag::SOCK_CLOEXEC,
        None,
    )?)
}

pub struct Listener(OwnedFd);

impl Listener {
    pub fn bind(path: &Path) -> Result<Self> {
        let socket = seqpacket()?;
        bind(socket.as_raw_fd(), &UnixAddr::new(path)?)?;
        listen(&socket, Backlog::new(16)?)?;
        Ok(Self(socket))
    }

    pub fn accept(&self) -> Result<Channel> {
        let raw = accept4(self.0.as_raw_fd(), SockFlag::SOCK_CLOEXEC)?;
        // accept4 returns a new descriptor that nothing else owns.
        Ok(Channel::from(unsafe { OwnedFd::from_raw_fd(raw) }))
    }
}

// SOCK_SEQPACKET keeps message boundaries like datagrams but reports peer death as EOF.
pub struct Channel(UnixStream);

impl Channel {
    pub fn connect(path: &Path) -> Result<Self> {
        let socket = seqpacket()?;
        connect(socket.as_raw_fd(), &UnixAddr::new(path)?)?;
        Ok(Self::from(socket))
    }

    pub fn pair() -> Result<(Self, Self)> {
        let (first, second) = socketpair(
            AddressFamily::Unix,
            SockType::SeqPacket,
            None,
            SockFlag::SOCK_CLOEXEC,
        )?;
        Ok((Self::from(first), Self::from(second)))
    }

    pub fn try_clone(&self) -> Result<Self> {
        Ok(Self(self.0.try_clone()?))
    }

    pub fn set_timeout(&self, timeout: Option<Duration>) -> Result<()> {
        self.0.set_read_timeout(timeout)?;
        self.0.set_write_timeout(timeout)?;
        Ok(())
    }

    pub fn set_nonblocking(&self) -> Result<()> {
        Ok(self.0.set_nonblocking(true)?)
    }

    pub fn send<T: Serialize>(&self, value: &T, descriptors: &[BorrowedFd<'_>]) -> Result<()> {
        let bytes = serde_json::to_vec(value)?;
        if bytes.len() > MAX_MESSAGE || descriptors.len() > MAX_DESCRIPTORS {
            bail!("IPC message is too large");
        }
        let raw: Vec<RawFd> = descriptors.iter().map(AsRawFd::as_raw_fd).collect();
        let rights = [ControlMessage::ScmRights(&raw)];
        let messages = if raw.is_empty() { &[][..] } else { &rights[..] };
        sendmsg::<()>(
            self.0.as_raw_fd(),
            &[IoSlice::new(&bytes)],
            messages,
            MsgFlags::MSG_NOSIGNAL,
            None,
        )
        .context("peer process is gone")?;
        Ok(())
    }

    // Returns None when the peer closed its end.
    pub fn receive<T: DeserializeOwned>(&self) -> Result<Option<(T, Vec<OwnedFd>)>> {
        let mut bytes = vec![0; MAX_MESSAGE];
        let mut ancillary = nix::cmsg_space!([RawFd; MAX_DESCRIPTORS]);
        let mut slices = [IoSliceMut::new(&mut bytes)];
        let message = recvmsg::<()>(
            self.0.as_raw_fd(),
            &mut slices,
            Some(&mut ancillary),
            MsgFlags::MSG_CMSG_CLOEXEC,
        )?;
        let length = message.bytes;
        let truncated = message
            .flags
            .intersects(MsgFlags::MSG_TRUNC | MsgFlags::MSG_CTRUNC);
        let mut descriptors = Vec::new();
        for control in message.cmsgs()? {
            if let ControlMessageOwned::ScmRights(received) = control {
                // SCM_RIGHTS transfers ownership of each received descriptor.
                descriptors.extend(
                    received
                        .into_iter()
                        .map(|raw| unsafe { OwnedFd::from_raw_fd(raw) }),
                );
            }
        }
        if truncated {
            bail!("truncated IPC message");
        }
        if length == 0 {
            return Ok(None);
        }
        Ok(Some((
            serde_json::from_slice(&bytes[..length])?,
            descriptors,
        )))
    }

    pub fn expect<T: DeserializeOwned>(&self) -> Result<(T, Vec<OwnedFd>)> {
        self.receive()?.context("peer process exited")
    }
}

impl From<OwnedFd> for Channel {
    fn from(descriptor: OwnedFd) -> Self {
        Self(UnixStream::from(descriptor))
    }
}

impl From<UnixStream> for Channel {
    fn from(stream: UnixStream) -> Self {
        Self(stream)
    }
}

impl From<Channel> for OwnedFd {
    fn from(channel: Channel) -> Self {
        channel.0.into()
    }
}

impl AsFd for Channel {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl AsRawFd for Channel {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

pub fn exact<const N: usize>(descriptors: Vec<OwnedFd>) -> Result<[OwnedFd; N]> {
    let count = descriptors.len();
    descriptors
        .try_into()
        .map_err(|_| anyhow::anyhow!("expected {N} descriptors, received {count}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn messages_carry_descriptors() {
        let (sender, receiver) = Channel::pair().unwrap();
        let (mut peer, descriptor) = UnixStream::pair().unwrap();
        sender.send(&"ready", &[descriptor.as_fd()]).unwrap();
        let (value, descriptors) = receiver.expect::<String>().unwrap();
        assert_eq!(value, "ready");
        let [descriptor] = exact::<1>(descriptors).unwrap();
        UnixStream::from(descriptor).write_all(b"ok").unwrap();
        let mut response = [0; 2];
        peer.read_exact(&mut response).unwrap();
        assert_eq!(&response, b"ok");
    }

    #[test]
    fn closed_peer_is_reported_immediately() {
        let (sender, receiver) = Channel::pair().unwrap();
        drop(sender);
        receiver.set_timeout(Some(Duration::from_secs(5))).unwrap();
        let started = std::time::Instant::now();
        assert!(receiver.receive::<String>().unwrap().is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
