// Messages between the `waywarp` client and an instance supervisor.
use crate::bridge::Subnets;
use crate::location::{Constraints, Locations};
use crate::store::{Instance, Name};
use crate::via::interface::Interface;
use crate::via::{Credentials, Via};
use crate::warp::State;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::SocketAddrV4;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "type")]
pub enum Access {
    Bridge {
        link: String,
        subnets: Subnets,
        #[serde(default)]
        nat: crate::bridge::nat::Mode,
    },
    Proxy {
        listen: SocketAddrV4,
    },
}

impl fmt::Display for Access {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bridge { link, subnets, .. } => write!(formatter, "bridge {link} {subnets}"),
            Self::Proxy { listen } => write!(formatter, "proxy {listen}"),
        }
    }
}

// Everything the supervisor needs, resolved on the host by the invoking command.
#[derive(Debug, Serialize, Deserialize)]
pub struct Plan {
    pub instance: Instance,
    pub name: Option<Name>,
    pub access: Access,
    pub interface: Interface,
    pub edge: SocketAddrV4,
    // Team registrations reach Cloudflare over TCP as well as through the tunnel.
    pub redirect_tcp: bool,
    pub location: Constraints,
    pub via: Vec<Via>,
    pub credentials: Credentials,
    pub mudfish_port: u16,
    pub rebootstrap: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    pub index: u8,
    pub name: Option<Name>,
    pub access: Access,
    pub state: State,
    pub locations: Locations,
    // The relay the current tunnel bootstrapped through, if any.
    pub relay: Option<String>,
    pub matched: bool,
    pub rebootstraps: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nat: Option<crate::bridge::nat::Policy>,
}

impl fmt::Display for Status {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.index)?;
        if let Some(name) = &self.name {
            write!(formatter, " {name}")?;
        }
        write!(
            formatter,
            ": {}, {}, {}",
            self.state, self.access, self.locations
        )?;
        if let Some(nat) = &self.nat {
            let routed: Vec<_> = nat.routed.iter().map(ToString::to_string).collect();
            let routed = if routed.is_empty() {
                "none".into()
            } else {
                routed.join(" ")
            };
            write!(formatter, ", nat {}, routed {routed}", nat.mode)?;
            if !nat.configuration_valid {
                formatter.write_str(" (registration unreadable; using last verified addresses)")?;
            }
        }
        if let Some(relay) = &self.relay {
            write!(formatter, ", bootstrapped via {relay}")?;
        }
        match self.rebootstraps {
            0 => {}
            1 => formatter.write_str(", 1 rebootstrap")?,
            count => write!(formatter, ", {count} rebootstraps")?,
        }
        if !self.matched {
            formatter.write_str(", location mismatched")?;
        }
        Ok(())
    }
}

// Sent over the setup channel while `up` waits.
#[derive(Debug, Serialize, Deserialize)]
pub enum Event {
    Progress(String),
    Up(Box<Status>),
    Failed(String),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Status,
    // Carries stdout and stderr descriptors for warp-cli.
    WarpCli(Vec<String>),
    Stop,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Status(Box<Status>),
    Exit(i32),
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::nat::{Mode, Policy};

    #[test]
    fn bridge_status_reads_as_text() {
        let mut status = Status {
            index: 2,
            name: Some("hong-kong".parse().unwrap()),
            access: Access::Bridge {
                link: "waywarp2".into(),
                subnets: Subnets::new(2, None, None),
                nat: Mode::Auto,
            },
            state: State::Connected,
            locations: Locations::default(),
            relay: None,
            matched: true,
            rebootstraps: 0,
            nat: Some(Policy {
                mode: Mode::Auto,
                routed: vec![
                    "10.42.0.0/16".parse().unwrap(),
                    "fd42::/64".parse().unwrap(),
                ],
                configuration_valid: true,
                ..Policy::default()
            }),
        };
        let text = status.to_string();
        assert!(text.starts_with("2 hong-kong: connected, bridge waywarp2"));
        assert!(text.contains(", nat auto, routed 10.42.0.0/16 fd42::/64"));
        status.nat = Some(Policy::default());
        assert!(
            status
                .to_string()
                .contains(", nat auto, routed none (registration unreadable")
        );
    }
}
