// Joins two TCP streams, copying each direction through its own pipe.
use anyhow::{Result, bail};
use mio::net::TcpStream;
use mio::{Interest, Registry, Token};
use slab::Slab;
use std::io::{ErrorKind, Read, Write};
use std::net::Shutdown;
use tracing::debug;

const BUFFER: usize = 64 * 1024;
const MAX_SPLICES: usize = 4096;

// The buffer exists only while bytes are in flight, so idle connections cost no memory.
#[derive(Default)]
struct Pipe {
    bytes: Option<Box<[u8]>>,
    start: usize,
    end: usize,
    closed: bool,
    shut: bool,
}

impl Pipe {
    fn done(&self) -> bool {
        self.closed && self.start == self.end
    }

    // Each returns whether it moved any bytes, so the caller can loop until both sides would block.
    fn fill(&mut self, from: &mut TcpStream) -> std::io::Result<bool> {
        let mut progressed = false;
        while !self.closed && self.end < BUFFER {
            let bytes = self
                .bytes
                .get_or_insert_with(|| vec![0; BUFFER].into_boxed_slice());
            match from.read(&mut bytes[self.end..]) {
                Ok(0) => self.closed = true,
                Ok(length) => self.end += length,
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
            progressed = true;
        }
        Ok(progressed)
    }

    fn drain(&mut self, to: &mut TcpStream) -> std::io::Result<bool> {
        let mut progressed = false;
        while self.start < self.end {
            let bytes = self.bytes.as_deref().unwrap_or_default();
            match to.write(&bytes[self.start..self.end]) {
                Ok(0) => return Err(ErrorKind::WriteZero.into()),
                Ok(length) => self.start += length,
                Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(progressed),
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
            progressed = true;
        }
        self.start = 0;
        self.end = 0;
        if self.closed && !self.shut {
            self.shut = true;
            to.shutdown(Shutdown::Write)?;
        }
        Ok(progressed)
    }
}

// Copies one accepted connection to one outgoing connection, preserving half-closes.
struct Splice {
    sockets: [TcpStream; 2],
    pipes: [Pipe; 2],
    connected: bool,
}

impl Splice {
    // Edge-triggered readiness: every event drains both directions until they would block.
    fn advance(&mut self) -> std::io::Result<bool> {
        if !self.connected {
            if let Some(error) = self.sockets[1].take_error()? {
                return Err(error);
            }
            match self.sockets[1].peer_addr() {
                Ok(_) => self.connected = true,
                Err(error) if error.kind() == ErrorKind::NotConnected => return Ok(false),
                Err(error) => return Err(error),
            }
        }
        loop {
            let mut progressed = false;
            for direction in 0..2 {
                let [first, second] = &mut self.sockets;
                let (from, to) = if direction == 0 {
                    (first, second)
                } else {
                    (second, first)
                };
                progressed |= self.pipes[direction].fill(from)?;
                progressed |= self.pipes[direction].drain(to)?;
            }
            if !progressed {
                break;
            }
        }
        // Every pipe that drained has nothing in flight until the next readiness event.
        for pipe in &mut self.pipes {
            if pipe.start == pipe.end {
                pipe.bytes = None;
            }
        }
        Ok(self.pipes.iter().all(Pipe::done))
    }
}

pub struct Splices {
    sessions: Slab<Splice>,
    base: usize,
}

impl Splices {
    pub fn new(base: usize) -> Self {
        Self {
            sessions: Slab::new(),
            base,
        }
    }

    pub fn owns(&self, token: Token) -> bool {
        token.0 >= self.base
    }

    pub fn insert(
        &mut self,
        registry: &Registry,
        incoming: TcpStream,
        outgoing: TcpStream,
    ) -> Result<()> {
        incoming.set_nodelay(true)?;
        outgoing.set_nodelay(true)?;
        if self.sessions.len() >= MAX_SPLICES {
            bail!("too many TCP connections");
        }
        let entry = self.sessions.vacant_entry();
        let key = entry.key();
        let mut splice = Splice {
            sockets: [incoming, outgoing],
            pipes: Default::default(),
            connected: false,
        };
        let interest = Interest::READABLE | Interest::WRITABLE;
        for (side, socket) in splice.sockets.iter_mut().enumerate() {
            registry.register(socket, Token(self.base + key * 2 + side), interest)?;
        }
        entry.insert(splice);
        Ok(())
    }

    pub fn ready(&mut self, registry: &Registry, token: Token) {
        let key = (token.0 - self.base) / 2;
        let Some(splice) = self.sessions.get_mut(key) else {
            return;
        };
        let finished = match splice.advance() {
            Ok(finished) => finished,
            Err(error) => {
                if !matches!(
                    error.kind(),
                    ErrorKind::ConnectionReset | ErrorKind::BrokenPipe
                ) {
                    debug!(%error, "TCP splice closed");
                }
                true
            }
        };
        if finished {
            let mut splice = self.sessions.remove(key);
            for socket in &mut splice.sockets {
                let _ = registry.deregister(socket);
            }
        }
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mio::{Events, Poll};
    use std::net::TcpListener;
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn splice_forwards_both_directions_and_half_closes() {
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = target.local_addr().unwrap();
        let payload: Vec<u8> = (0..1_000_000u32).map(|value| value as u8).collect();
        let expected = payload.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = target.accept().unwrap();
            let mut request = Vec::new();
            stream.read_to_end(&mut request).unwrap();
            assert_eq!(request, expected);
            stream.write_all(b"response").unwrap();
        });
        let ingress = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = std::net::TcpStream::connect(ingress.local_addr().unwrap()).unwrap();
        let (incoming, _) = ingress.accept().unwrap();
        incoming.set_nonblocking(true).unwrap();
        let incoming = TcpStream::from_std(incoming);
        let mut poll = Poll::new().unwrap();
        let mut splices = Splices::new(16);
        splices
            .insert(
                poll.registry(),
                incoming,
                TcpStream::connect(address).unwrap(),
            )
            .unwrap();
        let worker = thread::spawn(move || {
            let started = Instant::now();
            let mut events = Events::with_capacity(16);
            while !splices.is_empty() && started.elapsed() < Duration::from_secs(10) {
                poll.poll(&mut events, Some(Duration::from_millis(50)))
                    .unwrap();
                for event in &events {
                    splices.ready(poll.registry(), event.token());
                }
            }
            assert!(splices.is_empty(), "splice did not finish");
        });
        client.write_all(&payload).unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).unwrap();
        assert_eq!(response, b"response");
        server.join().unwrap();
        worker.join().unwrap();
    }
}
