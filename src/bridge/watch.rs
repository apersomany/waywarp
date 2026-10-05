use crate::warp::LINK;
use anyhow::{Context, Result};
use nix::errno::Errno;
use nix::libc;
use nix::sys::socket::{
    AddressFamily, MsgFlags, NetlinkAddr, SockFlag, SockProtocol, SockType, bind, recv, socket,
};
use std::os::fd::{AsRawFd, OwnedFd};

pub struct LinkEvents {
    socket: OwnedFd,
    tracker: Tracker,
}

impl LinkEvents {
    // Subscribe before warp-svc starts, so link recreation and address changes cannot fall in
    // the gap between the initial bridge snapshot and its background watcher.
    pub fn open() -> Result<Self> {
        let socket = socket(
            AddressFamily::Netlink,
            SockType::Raw,
            SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
            SockProtocol::NetlinkRoute,
        )?;
        let groups =
            (libc::RTMGRP_LINK | libc::RTMGRP_IPV4_IFADDR | libc::RTMGRP_IPV6_IFADDR) as u32;
        bind(socket.as_raw_fd(), &NetlinkAddr::new(0, groups))
            .context("subscribing to WARP link changes")?;
        Ok(Self {
            socket,
            tracker: Tracker::default(),
        })
    }

    pub fn changed(&mut self) -> Result<bool> {
        let mut buffer = [0; 16384];
        let mut changed = false;
        for _ in 0..64 {
            match recv(
                self.socket.as_raw_fd(),
                &mut buffer,
                MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_TRUNC,
            ) {
                Ok(0) => break,
                Ok(length) if length > buffer.len() => changed |= self.tracker.invalidate(),
                Ok(length) => changed |= self.tracker.observe(&buffer[..length]),
                Err(Errno::EAGAIN) => break,
                Err(Errno::EINTR) => continue,
                // An overflow means the current kernel snapshot, not the event stream, wins.
                Err(Errno::ENOBUFS) => changed |= self.tracker.invalidate(),
                Err(error) => return Err(error).context("reading WARP link changes"),
            }
        }
        Ok(changed)
    }
}

#[derive(Default)]
struct Tracker {
    index: Option<u32>,
}

fn aligned(length: usize) -> usize {
    (length + 3) & !3
}

fn link_name(mut attributes: &[u8]) -> Result<Option<&[u8]>, ()> {
    while !attributes.is_empty() {
        if attributes.len() < 4 {
            return Err(());
        }
        let length = usize::from(u16::from_ne_bytes(attributes[..2].try_into().unwrap()));
        let kind = u16::from_ne_bytes(attributes[2..4].try_into().unwrap()) & 0x3fff;
        if length < 4 || length > attributes.len() {
            return Err(());
        }
        let attribute = &attributes[..length];
        if kind == libc::IFLA_IFNAME {
            return Ok(Some(
                attribute[4..].strip_suffix(&[0]).unwrap_or(&attribute[4..]),
            ));
        }
        let step = aligned(length);
        if step > attributes.len() {
            if length != attributes.len() {
                return Err(());
            }
            attributes = &[];
        } else {
            attributes = &attributes[step..];
        }
    }
    Ok(None)
}

impl Tracker {
    fn invalidate(&mut self) -> bool {
        self.index = None;
        true
    }

    fn observe(&mut self, mut datagram: &[u8]) -> bool {
        let mut changed = false;
        while !datagram.is_empty() {
            if datagram.len() < 16 {
                return self.invalidate();
            }
            let length = u32::from_ne_bytes(datagram[..4].try_into().unwrap()) as usize;
            if !(16..=datagram.len()).contains(&length) {
                return self.invalidate();
            }
            let kind = u16::from_ne_bytes(datagram[4..6].try_into().unwrap());
            let message = &datagram[16..length];
            match kind {
                libc::RTM_NEWLINK | libc::RTM_DELLINK if message.len() >= 16 => {
                    let index = u32::from_ne_bytes(message[4..8].try_into().unwrap());
                    let name = match link_name(&message[16..]) {
                        Ok(name) => name,
                        Err(()) => return self.invalidate(),
                    };
                    if name == Some(LINK.as_bytes()) || self.index == Some(index) {
                        changed = true;
                        self.index = if kind == libc::RTM_NEWLINK && name == Some(LINK.as_bytes()) {
                            Some(index)
                        } else {
                            None
                        };
                    }
                }
                libc::RTM_NEWADDR | libc::RTM_DELADDR if message.len() >= 8 => {
                    let index = u32::from_ne_bytes(message[4..8].try_into().unwrap());
                    changed |= self.index.is_none_or(|warp_index| warp_index == index);
                }
                kind if kind == libc::NLMSG_OVERRUN as u16 || kind == libc::NLMSG_ERROR as u16 => {
                    return self.invalidate();
                }
                libc::RTM_NEWLINK | libc::RTM_DELLINK | libc::RTM_NEWADDR | libc::RTM_DELADDR => {
                    return self.invalidate();
                }
                _ => {}
            }
            let step = aligned(length);
            if step > datagram.len() {
                if length != datagram.len() {
                    return self.invalidate();
                }
                datagram = &[];
            } else {
                datagram = &datagram[step..];
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(kind: u16, payload: &[u8]) -> Vec<u8> {
        let mut message = ((16 + payload.len()) as u32).to_ne_bytes().to_vec();
        message.extend_from_slice(&kind.to_ne_bytes());
        message.extend_from_slice(&[0; 10]);
        message.extend_from_slice(payload);
        message.resize(aligned(message.len()), 0);
        message
    }

    fn link(kind: u16, index: u32, name: &str) -> Vec<u8> {
        let mut payload = vec![0; 16];
        payload[4..8].copy_from_slice(&index.to_ne_bytes());
        payload.extend_from_slice(&((5 + name.len()) as u16).to_ne_bytes());
        payload.extend_from_slice(&libc::IFLA_IFNAME.to_ne_bytes());
        payload.extend_from_slice(name.as_bytes());
        payload.push(0);
        message(kind, &payload)
    }

    fn address(index: u32) -> Vec<u8> {
        let mut payload = vec![0; 8];
        payload[4..8].copy_from_slice(&index.to_ne_bytes());
        message(libc::RTM_NEWADDR, &payload)
    }

    #[test]
    fn only_warp_link_and_address_events_trigger_reconciliation() {
        let mut tracker = Tracker::default();
        assert!(!tracker.observe(&link(libc::RTM_NEWLINK, 1, "veth")));
        assert!(tracker.observe(&link(libc::RTM_NEWLINK, 2, LINK)));
        assert!(!tracker.observe(&address(1)));
        assert!(tracker.observe(&address(2)));
        assert!(!tracker.observe(&link(libc::RTM_NEWLINK, 1, "veth")));
    }

    #[test]
    fn link_recreation_tracks_the_new_index() {
        let mut tracker = Tracker::default();
        tracker.observe(&link(libc::RTM_NEWLINK, 2, LINK));
        assert!(tracker.observe(&link(libc::RTM_DELLINK, 2, LINK)));
        assert!(tracker.observe(&link(libc::RTM_NEWLINK, 3, LINK)));
        assert!(!tracker.observe(&address(2)));
        assert!(tracker.observe(&address(3)));
    }

    #[test]
    fn lost_events_invalidate_the_index_until_warp_link_is_seen() {
        for loss in [
            message(libc::NLMSG_OVERRUN as u16, &[]),
            vec![0; 15],
            message(libc::RTM_NEWLINK, &[0; 1]),
            {
                let mut malformed = link(libc::RTM_NEWLINK, 3, "veth");
                let attribute_offset = 16 + 16;
                malformed[attribute_offset..attribute_offset + 2]
                    .copy_from_slice(&u16::MAX.to_ne_bytes());
                malformed
            },
        ] {
            let mut tracker = Tracker::default();
            tracker.observe(&link(libc::RTM_NEWLINK, 2, LINK));
            assert!(tracker.observe(&loss));
            assert!(tracker.observe(&address(3)));
            assert!(tracker.observe(&link(libc::RTM_NEWLINK, 3, LINK)));
            assert!(!tracker.observe(&address(2)));
            assert!(!tracker.observe(&address(4)));
        }
    }

    #[test]
    fn coalesced_messages_and_overflows_are_handled() {
        let mut tracker = Tracker::default();
        let mut batch = link(libc::RTM_NEWLINK, 1, "lo");
        batch.extend_from_slice(&link(libc::RTM_NEWLINK, 4, LINK));
        batch.extend_from_slice(&address(4));
        assert!(tracker.observe(&batch));
        assert_eq!(tracker.index, Some(4));
        assert!(tracker.observe(&message(libc::NLMSG_OVERRUN as u16, &[])));
        assert_eq!(tracker.index, None);
        for length in 1..16 {
            assert!(tracker.observe(&[0; 16][..length]));
        }
        assert!(tracker.observe(&message(libc::RTM_NEWLINK, &[])));
    }
}
