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
    #[serde(default)]
    pub accept_tos: bool,
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

impl Status {
    pub fn healthy(&self) -> bool {
        self.state == State::Connected && self.matched
    }
}

// Preserve error boundaries across IPC instead of joining the whole chain with colons.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(from = "ReceivedFailure")]
pub struct Failure {
    pub message: String,
    pub causes: Vec<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ReceivedFailure {
    Structured {
        message: String,
        causes: Vec<String>,
    },
    Legacy(String),
}

impl From<ReceivedFailure> for Failure {
    fn from(failure: ReceivedFailure) -> Self {
        match failure {
            ReceivedFailure::Structured { message, causes } => Self { message, causes },
            ReceivedFailure::Legacy(message) => Self {
                message,
                causes: Vec::new(),
            },
        }
    }
}

impl From<&anyhow::Error> for Failure {
    fn from(error: &anyhow::Error) -> Self {
        let mut chain = error.chain();
        Self {
            message: chain.next().expect("error has a message").to_string(),
            causes: chain.map(ToString::to_string).collect(),
        }
    }
}

impl Failure {
    pub fn into_error(self) -> anyhow::Error {
        let mut messages = std::iter::once(self.message).chain(self.causes).rev();
        let mut error = anyhow::Error::msg(messages.next().expect("failure has a message"));
        for message in messages {
            error = error.context(message);
        }
        error
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Progress {
    Registering,
    StartingDaemon,
    RelayProbed {
        relay: String,
        millis: u128,
        colo: Option<String>,
    },
    RelayProbeFailed {
        relay: String,
        failure: Failure,
    },
    Migrating,
    CheckingLocations,
    Connecting {
        route: String,
    },
    AttemptFailed {
        route: String,
        failure: Failure,
    },
}

// Sent over the setup channel while `up` waits.
#[derive(Debug, Serialize, Deserialize)]
pub enum Event {
    Progress(Progress),
    Up(Box<Status>),
    Failed(Failure),
}

#[derive(Serialize, Deserialize)]
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
    Failed(Failure),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_round_trip_preserves_the_error_chain() {
        let error = anyhow::anyhow!("connection refused")
            .context("opening a relay")
            .context("starting instance 2");
        let wire = serde_json::to_string(&Event::Failed(Failure::from(&error))).unwrap();
        let Event::Failed(failure) = serde_json::from_str(&wire).unwrap() else {
            panic!("expected failure");
        };
        let restored = failure.into_error();
        assert_eq!(
            restored
                .chain()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            [
                "starting instance 2",
                "opening a relay",
                "connection refused"
            ]
        );
    }

    #[test]
    fn older_supervisors_can_still_send_string_failures() {
        let Response::Failed(failure) =
            serde_json::from_str(r#"{"Failed":"connection refused"}"#).unwrap()
        else {
            panic!("expected failure");
        };
        assert_eq!(failure.message, "connection refused");
        assert!(failure.causes.is_empty());
    }
}
