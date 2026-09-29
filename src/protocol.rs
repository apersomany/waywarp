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
    Bridge { link: String, subnets: Subnets },
    Proxy { listen: SocketAddrV4 },
}

impl fmt::Display for Access {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bridge { link, subnets } => write!(formatter, "bridge {link} {subnets}"),
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
