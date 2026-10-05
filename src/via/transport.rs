use super::interface::Interface;
use crate::dataplane::Observer;
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, poll};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{SocketAddrV4, TcpStream};
use std::os::fd::{AsFd, BorrowedFd};
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const TIMEOUT: Duration = Duration::from_secs(5);

pub(super) fn check_cancelled(observer: &Observer) -> io::Result<()> {
    if observer.stopped() {
        // Read::read_exact and Write::write_all retry Interrupted errors indefinitely.
        return Err(io::Error::new(
            ErrorKind::ConnectionAborted,
            "relay operation was cancelled",
        ));
    }
    Ok(())
}

fn wait_socket(
    descriptor: BorrowedFd<'_>,
    interest: PollFlags,
    deadline: Instant,
    observer: &Observer,
) -> io::Result<()> {
    let mut descriptors = [PollFd::new(descriptor, interest)];
    loop {
        check_cancelled(observer)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                ErrorKind::TimedOut,
                "relay operation timed out",
            ));
        }
        let millis = remaining.min(POLL_INTERVAL).as_millis().max(1) as u16;
        match poll(&mut descriptors, millis) {
            Ok(0) | Err(Errno::EINTR) => continue,
            Ok(_) => {
                check_cancelled(observer)?;
                if descriptors[0]
                    .revents()
                    .is_some_and(|events| events.contains(PollFlags::POLLNVAL))
                {
                    return Err(Errno::EBADF.into());
                }
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        }
    }
}

pub(super) fn socket_io<T>(
    descriptor: BorrowedFd<'_>,
    interest: PollFlags,
    deadline: Instant,
    observer: &Observer,
    mut operation: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    loop {
        check_cancelled(observer)?;
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                ErrorKind::TimedOut,
                "relay operation timed out",
            ));
        }
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                wait_socket(descriptor, interest, deadline, observer)?;
            }
            Err(error) => return Err(error),
        }
    }
}

pub struct Stream {
    socket: TcpStream,
    observer: Observer,
    timeout: Duration,
}

impl Stream {
    pub fn connect(
        interface: &Interface,
        destination: SocketAddrV4,
        observer: &Observer,
    ) -> anyhow::Result<Self> {
        check_cancelled(observer)?;
        let deadline = Instant::now() + TIMEOUT;
        let socket = interface.tcp(destination)?;
        wait_socket(socket.as_fd(), PollFlags::POLLOUT, deadline, observer)?;
        if let Some(error) = socket.take_error()? {
            return Err(error.into());
        }
        socket.peer_addr()?;
        Ok(Self {
            socket,
            observer: observer.clone(),
            timeout: TIMEOUT,
        })
    }

    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    pub fn into_inner(self) -> TcpStream {
        self.socket
    }
}

impl Read for Stream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        socket_io(
            self.socket.as_fd(),
            PollFlags::POLLIN,
            Instant::now() + self.timeout,
            &self.observer,
            || (&self.socket).read(buffer),
        )
    }
}

impl Write for Stream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        socket_io(
            self.socket.as_fd(),
            PollFlags::POLLOUT,
            Instant::now() + self.timeout,
            &self.observer,
            || (&self.socket).write(bytes),
        )
    }

    fn flush(&mut self) -> io::Result<()> {
        check_cancelled(&self.observer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, UdpSocket};
    use std::sync::mpsc;
    use std::thread;

    fn connected(observer: &Observer) -> (Stream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let std::net::SocketAddr::V4(address) = listener.local_addr().unwrap() else {
            unreachable!()
        };
        let stream = Stream::connect(&Interface::Named("lo".into()), address, observer).unwrap();
        let (peer, _) = listener.accept().unwrap();
        (stream, peer)
    }

    #[test]
    fn stalled_reads_keep_their_deadline() {
        let (mut stream, _peer) = connected(&Observer::default());
        stream.set_timeout(Duration::from_millis(30));
        let started = Instant::now();
        assert_eq!(
            stream.read_exact(&mut [0]).unwrap_err().kind(),
            ErrorKind::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn cancellation_interrupts_a_backpressured_write_all() {
        let observer = Observer::default();
        let (mut stream, peer) = connected(&observer);
        socket2::SockRef::from(&stream.socket)
            .set_send_buffer_size(4096)
            .unwrap();
        let (finished, result) = mpsc::channel();
        let worker = thread::spawn(move || {
            finished
                .send(stream.write_all(&vec![0; 2 * 1024 * 1024]))
                .unwrap();
        });
        assert!(matches!(
            result.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        observer.stop();
        let completed = result.recv_timeout(Duration::from_secs(1));
        drop(peer);
        worker.join().unwrap();
        assert_eq!(
            completed.unwrap().unwrap_err().kind(),
            ErrorKind::ConnectionAborted
        );
    }

    #[test]
    fn cancellation_interrupts_a_udp_reply_wait() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let address = socket.local_addr().unwrap();
        let observer = Observer::default();
        let cancelled = observer.clone();
        let (finished, result) = mpsc::channel();
        let worker = thread::spawn(move || {
            finished
                .send(socket_io(
                    socket.as_fd(),
                    PollFlags::POLLIN,
                    Instant::now() + Duration::from_secs(5),
                    &cancelled,
                    || socket.recv(&mut [0]),
                ))
                .unwrap();
        });
        assert!(matches!(
            result.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        observer.stop();
        let completed = result.recv_timeout(Duration::from_secs(1));
        if completed.is_err() {
            UdpSocket::bind("127.0.0.1:0")
                .unwrap()
                .send_to(b"wake", address)
                .unwrap();
        }
        worker.join().unwrap();
        assert_eq!(
            completed.unwrap().unwrap_err().kind(),
            ErrorKind::ConnectionAborted
        );
    }
}
