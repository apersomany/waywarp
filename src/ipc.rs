use anyhow::{Context, Result, bail};
use nix::sys::socket::{
    AddressFamily, Backlog, ControlMessage, MsgFlags, SockFlag, SockType, UnixAddr, accept4, bind,
    connect, listen, sendmsg, socket, socketpair,
};
use nix::{errno::Errno, libc};
use serde::{Serialize, de::DeserializeOwned};
use std::io::IoSlice;
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

    pub fn set_nonblocking(&self) -> Result<()> {
        use nix::fcntl::{FcntlArg, OFlag, fcntl};
        let flags = OFlag::from_bits_truncate(fcntl(&self.0, FcntlArg::F_GETFL)?);
        fcntl(&self.0, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
        Ok(())
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
        // Word alignment permits reading cmsghdr and RawFd values from the control buffer.
        let mut ancillary = [0usize;
            nix::sys::socket::cmsg_space::<[RawFd; MAX_DESCRIPTORS]>()
                .div_ceil(std::mem::size_of::<usize>())];
        let mut iovec = libc::iovec {
            iov_base: bytes.as_mut_ptr().cast(),
            iov_len: bytes.len(),
        };
        // msghdr accepts null pointers for unused fields; the remaining pointers stay live
        // through recvmsg, which bounds its writes to the supplied buffer lengths.
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iovec;
        message.msg_iovlen = 1;
        message.msg_control = ancillary.as_mut_ptr().cast();
        // Control lengths are size_t on glibc and unsigned int on musl.
        message.msg_controllen = std::mem::size_of_val(&ancillary) as _;
        let length = Errno::result(unsafe {
            libc::recvmsg(self.0.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC)
        })? as usize;
        let truncated = message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0;
        let mut descriptors = Vec::new();
        // nix's cmsgs() refuses truncated control data, but Linux may have installed some
        // descriptors even then. Kernel-generated records contain complete, bounded FDs;
        // adopt every one before rejecting the message so all error paths close them.
        unsafe {
            let mut header = libc::CMSG_FIRSTHDR(&message);
            while !header.is_null() {
                if (*header).cmsg_level == libc::SOL_SOCKET
                    && (*header).cmsg_type == libc::SCM_RIGHTS
                {
                    let control_length: usize = (*header).cmsg_len as _;
                    let count = (control_length - libc::CMSG_LEN(0) as usize)
                        / std::mem::size_of::<RawFd>();
                    let rights =
                        std::slice::from_raw_parts(libc::CMSG_DATA(header).cast::<RawFd>(), count);
                    descriptors.extend(rights.iter().map(|&raw| OwnedFd::from_raw_fd(raw)));
                }
                header = libc::CMSG_NXTHDR(&message, header);
            }
        }
        if truncated {
            bail!("truncated IPC message");
        }
        if descriptors.len() > MAX_DESCRIPTORS {
            bail!("too many IPC descriptors");
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
        self.receive()?.with_context(|| {
            std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "peer process exited")
        })
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

    fn send_raw_rights(sender: &Channel, payload: &[u8], descriptor: RawFd, count: usize) {
        let raw = vec![descriptor; count];
        let rights = [ControlMessage::ScmRights(&raw)];
        sendmsg::<()>(
            sender.as_raw_fd(),
            &[IoSlice::new(payload)],
            &rights,
            MsgFlags::MSG_NOSIGNAL,
            None,
        )
        .unwrap();
    }

    fn assert_peer_eof(peer: &mut UnixStream) {
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut byte = [0];
        assert_eq!(peer.read(&mut byte).unwrap(), 0);
    }

    #[test]
    fn excess_raw_descriptors_are_closed() {
        for count in [MAX_DESCRIPTORS + 1, 253] {
            let (sender, receiver) = Channel::pair().unwrap();
            let (mut peer, descriptor) = UnixStream::pair().unwrap();
            send_raw_rights(&sender, b"\"ready\"", descriptor.as_raw_fd(), count);
            drop(descriptor);
            assert!(receiver.expect::<String>().is_err());
            assert_peer_eof(&mut peer);
        }
    }

    #[test]
    fn malformed_and_oversized_messages_close_rights() {
        for payload in [b"not json".to_vec(), vec![b' '; MAX_MESSAGE + 1]] {
            let (sender, receiver) = Channel::pair().unwrap();
            let (mut peer, descriptor) = UnixStream::pair().unwrap();
            send_raw_rights(&sender, &payload, descriptor.as_raw_fd(), 1);
            drop(descriptor);
            assert!(receiver.expect::<String>().is_err());
            assert_peer_eof(&mut peer);
        }
    }

    #[test]
    fn descriptor_limit_truncation_closes_received_rights() {
        const CHILD_ENV: &str = "WAYWARP_IPC_LIMIT_TEST";
        if std::env::var_os(CHILD_ENV).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "ipc::tests::descriptor_limit_truncation_closes_received_rights",
                ])
                .env(CHILD_ENV, "1")
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let (sender, receiver) = Channel::pair().unwrap();
        let (mut peer, descriptor) = UnixStream::pair().unwrap();
        send_raw_rights(
            &sender,
            b"\"ready\"",
            descriptor.as_raw_fd(),
            MAX_DESCRIPTORS,
        );
        drop(descriptor);
        // Leave one descriptor slot, forcing control truncation without overflowing its buffer.
        // This resource limit applies only to this exec-isolated test process.
        let ceiling = [sender.as_raw_fd(), receiver.as_raw_fd(), peer.as_raw_fd()]
            .into_iter()
            .max()
            .unwrap() as libc::rlim_t
            + 2;
        let limit = libc::rlimit {
            rlim_cur: ceiling,
            rlim_max: ceiling,
        };
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
        assert_eq!(
            receiver.expect::<String>().unwrap_err().to_string(),
            "truncated IPC message"
        );
        // Check while this process is alive: exiting would hide leaked descriptors.
        assert_peer_eof(&mut peer);
    }

    #[test]
    fn receive_timeouts_keep_their_errno_for_setup_cancellation() {
        let (_sender, receiver) = Channel::pair().unwrap();
        receiver
            .set_timeout(Some(Duration::from_millis(1)))
            .unwrap();
        let error = receiver.expect::<String>().unwrap_err();
        assert_eq!(error.downcast_ref::<Errno>(), Some(&Errno::EAGAIN));
    }

    #[test]
    fn closed_peer_is_reported_immediately() {
        let (sender, receiver) = Channel::pair().unwrap();
        drop(sender);
        receiver.set_timeout(Some(Duration::from_secs(5))).unwrap();
        let started = std::time::Instant::now();
        assert!(receiver.receive::<String>().unwrap().is_none());
        let error = receiver.expect::<String>().unwrap_err();
        assert_eq!(error.to_string(), "peer process exited");
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
