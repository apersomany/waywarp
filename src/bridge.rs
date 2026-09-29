// Bridge access: a dual-stack veth pair between the host and the private WARP namespace.
use crate::tool;
use crate::warp::LINK;
use anyhow::{Context, Result, bail};
use ipnet::{Ipv4Net, Ipv6Net};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;

// The namespace end of the veth pair and policy tables for bridged traffic.
const VETH: &str = "veth";
const WARP_TABLE: u32 = 79;
const HOST_TABLE: u32 = 80;
const DERIVED4: Ipv4Addr = Ipv4Addr::new(169, 254, 1, 0);
const DERIVED6: Ipv6Addr = Ipv6Addr::new(0xfd77, 0x6179, 0x7761, 0x7270, 0, 0, 0, 0);

pub fn link(index: u8) -> String {
    format!("waywarp{index}")
}

// A point-to-point subnet: the namespace side is the gateway, the host side is the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pair<A> {
    pub gateway: A,
    pub host: A,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subnets {
    pub v4: Ipv4Net,
    pub v6: Ipv6Net,
}

// Explicit subnets must be exactly one canonical point-to-point network.
pub fn parse4(value: &str) -> Result<Ipv4Net, String> {
    let subnet: Ipv4Net = value
        .parse()
        .map_err(|_| format!("{value:?} is not an IPv4 network"))?;
    if subnet.prefix_len() != 30 || subnet.trunc() != subnet {
        return Err(format!("{subnet} is not a canonical /30 network"));
    }
    Ok(subnet)
}

pub fn parse6(value: &str) -> Result<Ipv6Net, String> {
    let subnet: Ipv6Net = value
        .parse()
        .map_err(|_| format!("{value:?} is not an IPv6 network"))?;
    if subnet.prefix_len() != 126 || subnet.trunc() != subnet {
        return Err(format!("{subnet} is not a canonical /126 network"));
    }
    Ok(subnet)
}

impl Subnets {
    // Index N maps to the Nth /30 after 169.254.1.0 and the Nth /126 in Waywarp's ULA range, so
    // derived subnets never collide with each other.
    pub fn new(index: u8, v4: Option<Ipv4Net>, v6: Option<Ipv6Net>) -> Self {
        let offset = 4 * u32::from(index);
        Self {
            v4: v4
                .unwrap_or_else(|| Ipv4Net::new_assert((u32::from(DERIVED4) + offset).into(), 30)),
            v6: v6.unwrap_or_else(|| {
                Ipv6Net::new_assert((u128::from(DERIVED6) + u128::from(offset)).into(), 126)
            }),
        }
    }

    pub fn v4(&self) -> Pair<Ipv4Addr> {
        let start = u32::from(self.v4.network());
        Pair {
            gateway: (start + 1).into(),
            host: (start + 2).into(),
        }
    }

    pub fn v6(&self) -> Pair<Ipv6Addr> {
        let start = u128::from(self.v6.network());
        Pair {
            gateway: (start + 1).into(),
            host: (start + 2).into(),
        }
    }

    // Runs on the host before setup, so a bridge subnet never shadows or is shadowed by a host
    // route.
    pub fn check_routes(&self) -> Result<()> {
        for (family, subnet, flag) in [
            ("-4", ipnet::IpNet::V4(self.v4), "--subnet4"),
            ("-6", ipnet::IpNet::V6(self.v6), "--subnet6"),
        ] {
            let routes: Vec<serde_json::Value> = serde_json::from_str(&tool::run(
                "ip",
                &["-j", family, "route", "show", "table", "all"],
            )?)?;
            let conflict = routes.iter().find_map(|route| {
                let destination = route.get("dst")?.as_str()?;
                let network: ipnet::IpNet = destination
                    .parse()
                    .or_else(|_| destination.parse::<IpAddr>().map(ipnet::IpNet::from))
                    .ok()?;
                let overlaps = network.prefix_len() > 0
                    && (network.contains(&subnet) || subnet.contains(&network));
                overlaps.then(|| {
                    let device = route.get("dev").and_then(|value| value.as_str());
                    format!("{destination} dev {}", device.unwrap_or("?"))
                })
            });
            if let Some(route) = conflict {
                bail!(
                    "bridge subnet {subnet} overlaps the host route {route}; choose one with {flag}"
                );
            }
        }
        Ok(())
    }
}

impl fmt::Display for Subnets {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (v4, v6) = (self.v4(), self.v6());
        write!(
            formatter,
            "IPv4 {}/30 via {}, IPv6 {}/126 via {}",
            v4.host, v4.gateway, v6.host, v6.gateway
        )
    }
}

// Runs on the host: creates the host link and moves its peer into the private namespace.
pub fn attach(link: &str, subnets: Subnets, private: &Path) -> Result<()> {
    tool::run(
        "ip",
        &[
            "link",
            "add",
            link,
            "type",
            "veth",
            "peer",
            "name",
            VETH,
            "netns",
            &private.to_string_lossy(),
        ],
    )
    .with_context(|| format!("creating host link {link}"))?;
    tool::ip(
        "-4",
        &format!("address add {}/30 dev {link}", subnets.v4().host),
    )?;
    tool::ip(
        "-6",
        &format!(
            "address add {}/126 dev {link}
            link set {link} up",
            subnets.v6().host
        ),
    )
}

// Runs inside the namespace: both families and directions pass between the host and WARP. NAT
// presents WARP's addresses externally and maps traffic addressed to them onto the host link.
pub fn firewall(subnets: Subnets) -> Result<()> {
    let (v4, v6) = (subnets.v4(), subnets.v6());
    tool::nft(&format!(
        "table inet waywarp {{
            chain prerouting {{
                type nat hook prerouting priority dstnat;
                meta nfproto ipv4 iifname \"{LINK}\" fib daddr type local dnat ip to {}
                meta nfproto ipv6 iifname \"{LINK}\" fib daddr type local dnat ip6 to {}
            }}
            chain forward {{
                type filter hook forward priority filter; policy drop;
                iifname \"{VETH}\" oifname \"{LINK}\" accept
                iifname \"{LINK}\" oifname \"{VETH}\" accept
            }}
            chain postrouting {{
                type nat hook postrouting priority srcnat;
                iifname \"{VETH}\" oifname \"{LINK}\" masquerade
            }}
        }}",
        v4.host, v6.host
    ))
    .context("loading the bridge firewall")?;
    for (family, gateway) in [
        ("-4", format!("{}/30", v4.gateway)),
        ("-6", format!("{}/126", v6.gateway)),
    ] {
        tool::ip(
            family,
            &format!(
                "address add {gateway} dev {VETH}
                route add unreachable default table {WARP_TABLE} metric 4096
                rule add pref 100 iif {VETH} lookup {WARP_TABLE}"
            ),
        )?;
    }
    tool::ip("-4", &format!("link set {VETH} up"))?;
    fs::write("/proc/sys/net/ipv4/ip_forward", "1")?;
    fs::write("/proc/sys/net/ipv6/conf/all/forwarding", "1")?;
    Ok(())
}

// Runs inside the namespace once WARP is up, opening both directions between WARP and the host.
pub fn route(subnets: Subnets) -> Result<()> {
    for (family, host) in [
        ("-4", subnets.v4().host.to_string()),
        ("-6", subnets.v6().host.to_string()),
    ] {
        tool::ip(
            family,
            &format!(
                "route replace default dev {LINK} table {WARP_TABLE}
                route replace default via {host} dev {VETH} table {HOST_TABLE}
                rule add pref 101 iif {LINK} lookup {HOST_TABLE}"
            ),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_subnets_are_disjoint_and_stay_in_their_ranges() {
        let first = Subnets::new(0, None, None);
        assert_eq!(first.v4().gateway, Ipv4Addr::new(169, 254, 1, 1));
        assert_eq!(first.v4().host, Ipv4Addr::new(169, 254, 1, 2));
        let last = Subnets::new(255, None, None);
        assert_eq!(last.v4.to_string(), "169.254.4.252/30");
        assert_eq!(last.v6.to_string(), "fd77:6179:7761:7270::3fc/126");
        let second = Subnets::new(1, None, None);
        assert!(!first.v4.contains(&second.v4) && !first.v6.contains(&second.v6));
    }

    #[test]
    fn explicit_subnets_must_be_canonical() {
        assert!(parse4("10.9.0.4/30").is_ok());
        assert!(parse4("10.9.0.5/30").is_err());
        assert!(parse4("10.9.0.0/29").is_err());
        assert!(parse6("fd00::4/126").is_ok());
        assert!(parse6("fd00::5/126").is_err());
    }
}
