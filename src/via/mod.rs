pub mod interface;
pub mod mudfish;
pub mod ping;
pub mod socks5;

use anyhow::{Result, bail};
use mudfish::Filter;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::{IpAddr, SocketAddrV4};
use std::str::FromStr;

#[derive(Clone, Serialize, Deserialize)]
pub struct Login {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for Login {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Login([redacted])")
    }
}

// Relay logins, read once from the environment so they stay out of process listings.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Credentials {
    socks5: Option<Login>,
    mudfish: Option<Login>,
}

// Removes the variables it reads, so no helper process inherits them. Must run while the
// process is single-threaded.
fn take_login(service: &str, required: bool) -> Result<Option<Login>> {
    let take = |field: &str| {
        let variable = format!("WAYWARP_{service}_{field}");
        let value = std::env::var(&variable)
            .ok()
            .filter(|value| !value.is_empty());
        // Sound because the caller guarantees no other thread reads the environment.
        unsafe { std::env::remove_var(&variable) };
        value
    };
    match (take("USERNAME"), take("PASSWORD")) {
        (Some(username), Some(password)) => {
            if username.len() > 255 || password.len() > 255 {
                bail!("SOCKS5 credentials are limited to 255 bytes each");
            }
            Ok(Some(Login { username, password }))
        }
        (None, None) if !required => Ok(None),
        _ => bail!("set both WAYWARP_{service}_USERNAME and WAYWARP_{service}_PASSWORD"),
    }
}

impl Credentials {
    pub fn take(via: &[Via]) -> Result<Self> {
        let uses = |kind: fn(&Via) -> bool| via.iter().any(kind);
        let socks5 = take_login("SOCKS5", false)?;
        let mudfish = take_login("MUDFISH", uses(|via| matches!(via, Via::Mudfish(_))))?;
        Ok(Self { socks5, mudfish })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Relay {
    pub label: String,
    pub server: SocketAddrV4,
    pub login: Option<Login>,
}

// One concrete way to reach the edge, expanded from a via; a Mudfish via expands into many.
#[derive(Clone, Debug)]
pub enum Route {
    Direct,
    Relay(Relay),
}

impl fmt::Display for Route {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Direct => formatter.write_str("directly"),
            Self::Relay(relay) => write!(formatter, "via {}", relay.label),
        }
    }
}

// One `--via` entry as the user wrote it; a Mudfish filter expands into many relays.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Via {
    Direct,
    Socks5(SocketAddrV4),
    Mudfish(Filter),
}

impl FromStr for Via {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (kind, rest) = value.split_once(':').unwrap_or((value, ""));
        match kind.to_ascii_lowercase().as_str() {
            "direct" if rest.is_empty() => Ok(Self::Direct),
            "direct" => Err("direct takes no value".into()),
            "socks5" => {
                // socks5://ADDR is accepted because URIs get pasted; credentials stay out of argv.
                let address = rest
                    .strip_prefix("//")
                    .unwrap_or(rest)
                    .trim_end_matches('/');
                if address.contains('@') {
                    return Err("pass SOCKS5 credentials through WAYWARP_SOCKS5_USERNAME and WAYWARP_SOCKS5_PASSWORD".into());
                }
                address
                    .parse()
                    .map(Self::Socks5)
                    .map_err(|_| format!("socks5 needs an IPv4 ADDRESS:PORT, not {address:?}"))
            }
            "mudfish" => rest.parse().map(Self::Mudfish),
            _ => Err(format!(
                "unknown via {kind:?}; use direct, socks5:ADDRESS:PORT, or mudfish:FILTER"
            )),
        }
    }
}

impl fmt::Display for Via {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Direct => formatter.write_str("direct"),
            Self::Socks5(server) => write!(formatter, "socks5:{server}"),
            Self::Mudfish(filter) => write!(formatter, "mudfish:{filter}"),
        }
    }
}

impl TryFrom<String> for Via {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<Via> for String {
    fn from(via: Via) -> Self {
        via.to_string()
    }
}

pub fn validate(via: &[Via]) -> Result<()> {
    if via.iter().filter(|via| **via == Via::Direct).count() > 1 {
        bail!("--via direct may appear only once");
    }
    Ok(())
}

// Expands the ordered --via list into the routes to try. Mudfish filters expand in place from a
// freshly fetched node list, so every bootstrap sees current nodes.
pub fn expand(via: &[Via], credentials: &Credentials, mudfish_port: u16) -> Result<Vec<Route>> {
    let mut nodes = None;
    let seed = seed();
    let mut routes = Vec::new();
    for entry in via {
        match entry {
            Via::Direct => routes.push(Route::Direct),
            Via::Socks5(server) => routes.push(Route::Relay(Relay {
                label: server.to_string(),
                server: *server,
                login: credentials.socks5.clone(),
            })),
            Via::Mudfish(filter) => {
                let nodes = match &mut nodes {
                    Some(nodes) => nodes,
                    None => nodes.insert(mudfish::nodes()?),
                };
                let before = routes.len();
                let matched = nodes.iter().filter(|node| filter.matches(node));
                routes.extend(
                    mudfish::spread(matched, seed)
                        .into_iter()
                        .filter_map(|node| {
                            let IpAddr::V4(ip) = node.ip else {
                                return None;
                            };
                            Some(Route::Relay(Relay {
                                label: node.location.to_string(),
                                server: SocketAddrV4::new(ip, mudfish_port),
                                login: credentials.mudfish.clone(),
                            }))
                        }),
                );
                if routes.len() == before {
                    bail!("no IPv4 Mudfish node matches {filter}");
                }
            }
        }
    }
    if routes.is_empty() {
        routes.push(Route::Direct);
    }
    Ok(routes)
}

// Varies relay order and ping connection IDs between runs; not used for anything secret.
pub fn seed() -> u64 {
    use std::hash::{BuildHasher, RandomState};
    RandomState::new().hash_one(std::time::SystemTime::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socks5_accepts_pasted_uris_but_not_credentials() {
        let address = "192.0.2.1:1080".parse().unwrap();
        assert_eq!("SOCKS5://192.0.2.1:1080".parse(), Ok(Via::Socks5(address)));
        assert!("socks5://user:pass@192.0.2.1:1080".parse::<Via>().is_err());
    }
}
