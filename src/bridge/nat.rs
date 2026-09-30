// Source NAT follows the connector registration, not interface address selection. The latter
// can pick a shared connector address rather than the device's own address.
use crate::tool;
use crate::warp::LINK;
use anyhow::Result;
use clap::ValueEnum;
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Auto,
    Always,
    Never,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Never => "never",
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub mode: Mode,
    pub v4: Option<Ipv4Addr>,
    pub v6: Option<Ipv6Addr>,
    pub routed: Vec<IpNet>,
    pub configuration_valid: bool,
}

#[derive(Deserialize)]
struct Configuration {
    interface: Addresses,
    connector_config: Option<Connector>,
}

#[derive(Deserialize)]
struct Addresses {
    v4: Option<Ipv4Addr>,
    v6: Option<Ipv6Addr>,
}

#[derive(Deserialize)]
struct Connector {
    nat_mode: bool,
    routes: Vec<IpNet>,
}

impl Policy {
    // Never include parse errors or the configuration contents in logs: conf.json has secrets.
    // A broken/missing config removes exemptions, retaining only previously verified targets.
    pub fn read(mode: Mode, contents: Option<&[u8]>, previous: Option<&Self>) -> Self {
        let configuration =
            contents.and_then(|bytes| serde_json::from_slice::<Configuration>(bytes).ok());
        let Some(configuration) = configuration else {
            return Self {
                mode,
                v4: previous.and_then(|policy| policy.v4),
                v6: previous.and_then(|policy| policy.v6),
                routed: Vec::new(),
                configuration_valid: false,
            };
        };
        let mut routed = match (mode, configuration.connector_config) {
            (Mode::Auto, Some(connector)) if !connector.nat_mode => connector.routes,
            _ => Vec::new(),
        };
        routed.iter_mut().for_each(|route| *route = route.trunc());
        routed.sort();
        routed.dedup();
        Self {
            mode,
            v4: configuration.interface.v4,
            v6: configuration.interface.v6,
            routed,
            configuration_valid: true,
        }
    }

    // Only use targets that are actually present on WARP's current link. On reconnect the old
    // address may disappear before conf.json is refreshed; block that family rather than leak
    // untranslated traffic or choose an unrelated/shared address.
    pub fn verify_addresses(&mut self, addresses: &[IpAddr]) {
        self.v4 = self
            .v4
            .filter(|address| addresses.contains(&IpAddr::V4(*address)));
        self.v6 = self
            .v6
            .filter(|address| addresses.contains(&IpAddr::V6(*address)));
    }

    pub fn rules(&self, subnets: super::Subnets) -> String {
        let mut rules = String::from(
            "flush chain inet waywarp prerouting\nflush chain inet waywarp postrouting\n",
        );
        // Only the device addresses belong to the host. Additional connector addresses serve
        // namespace-local services (notably mesh DNS) and must not be redirected.
        if let Some(address) = self.v4 {
            rules.push_str(&format!("add rule inet waywarp prerouting iifname \"{LINK}\" ip daddr {address} dnat ip to {}\n", subnets.v4().host));
        }
        if let Some(address) = self.v6 {
            rules.push_str(&format!("add rule inet waywarp prerouting iifname \"{LINK}\" ip6 daddr {address} dnat ip6 to {}\n", subnets.v6().host));
        }
        let selector = format!("iifname \"{}\" oifname \"{LINK}\"", super::VETH);
        // An invalid config takes precedence over even a 'never' override: fail safe.
        if self.mode == Mode::Never && self.configuration_valid {
            return rules;
        }
        for route in &self.routed {
            let family = if matches!(route, IpNet::V4(_)) {
                "ip"
            } else {
                "ip6"
            };
            rules.push_str(&format!(
                "add rule inet waywarp postrouting {selector} {family} saddr {route} accept\n"
            ));
        }
        for (family, target) in [
            ("ipv4", self.v4.map(|ip| format!("snat ip to {ip}"))),
            ("ipv6", self.v6.map(|ip| format!("snat ip6 to {ip}"))),
        ] {
            rules.push_str(&format!(
                "add rule inet waywarp postrouting {selector} meta nfproto {family} {}\n",
                target.as_deref().unwrap_or("drop")
            ));
        }
        rules
    }

    pub fn apply(&self, subnets: super::Subnets) -> Result<()> {
        // One nft batch commits the flush and replacement together.
        tool::nft(&self.rules(subnets))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(policy: &Policy) -> String {
        policy.rules(super::super::Subnets::new(0, None, None))
    }

    const CONSUMER: &[u8] = br#"{"interface":{"v4":"172.16.0.2","v6":"2001:db8::2"}}"#;
    const MESH: &[u8] = br#"{"interface":{"v4":"100.96.0.37","v6":"2001:db8::37"},"connector_config":{"nat_mode":false,"routes":["10.0.0.0/16","fd00::/64"],"additional_interface_ips":["2001:db8::1"]}}"#;

    #[test]
    fn consumer_uses_only_assigned_addresses() {
        let policy = Policy::read(Mode::Auto, Some(CONSUMER), None);
        assert!(policy.routed.is_empty());
        assert!(rules(&policy).contains("snat ip to 172.16.0.2"));
        assert!(rules(&policy).contains("snat ip6 to 2001:db8::2"));
        assert!(!rules(&policy).contains("masquerade"));
    }

    #[test]
    fn connector_routes_and_overrides() {
        let policy = Policy::read(Mode::Auto, Some(MESH), None);
        assert_eq!(policy.routed.len(), 2);
        assert!(rules(&policy).contains("ip saddr 10.0.0.0/16 accept"));
        assert!(rules(&policy).contains("ip6 saddr fd00::/64 accept"));
        assert!(!rules(&policy).contains("snat ip6 to 2001:db8::1"));
        assert!(rules(&policy).contains("ip daddr 100.96.0.37 dnat ip to 169.254.1.2"));
        assert!(
            rules(&policy).contains("ip6 daddr 2001:db8::37 dnat ip6 to fd77:6179:7761:7270::2")
        );
        assert!(!rules(&policy).contains("ip6 daddr 2001:db8::1 "));
        assert!(!rules(&policy).contains("fib daddr"));
        let always = Policy::read(Mode::Always, Some(MESH), None);
        assert!(always.routed.is_empty());
        assert!(rules(&always).contains("snat"));
        let never = Policy::read(Mode::Never, Some(MESH), None);
        assert!(!rules(&never).contains("snat"));
        assert!(rules(&never).contains("dnat"));
        let enabled = String::from_utf8(MESH.to_vec())
            .unwrap()
            .replace("false", "true");
        assert!(
            Policy::read(Mode::Auto, Some(enabled.as_bytes()), None)
                .routed
                .is_empty()
        );
    }

    #[test]
    fn address_verification_is_independent_per_family_and_ignores_shared_addresses() {
        let mut policy = Policy::read(Mode::Auto, Some(MESH), None);
        policy.verify_addresses(&[
            "100.96.0.37".parse().unwrap(),
            "2001:db8::1".parse().unwrap(),
        ]);
        assert_eq!(policy.v4, Some("100.96.0.37".parse().unwrap()));
        assert_eq!(policy.v6, None);
        assert!(rules(&policy).contains("meta nfproto ipv6 drop"));
        assert!(!rules(&policy).contains("ip6 daddr"));
        assert!(rules(&policy).contains("ip6 saddr fd00::/64 accept"));
        policy.verify_addresses(&[]);
        assert_eq!(policy.v4, None);
        assert!(rules(&policy).contains("meta nfproto ipv4 drop"));
    }

    #[test]
    fn routed_subnets_are_canonical_and_unique() {
        let configuration = br#"{"interface":{},"connector_config":{"nat_mode":false,"routes":["10.0.1.1/16","10.0.0.0/16","fd00::1/64"]}}"#;
        let policy = Policy::read(Mode::Auto, Some(configuration), None);
        assert_eq!(
            policy.routed,
            [
                "10.0.0.0/16".parse::<IpNet>().unwrap(),
                "fd00::/64".parse().unwrap()
            ]
        );
    }

    #[test]
    fn invalid_configuration_removes_exemptions_and_fails_safe() {
        let old = Policy::read(Mode::Auto, Some(MESH), None);
        let invalid = Policy::read(Mode::Auto, Some(b"{"), Some(&old));
        assert!(!invalid.configuration_valid);
        assert!(invalid.routed.is_empty());
        assert_eq!(invalid.v4, old.v4);
        assert!(rules(&invalid).contains("snat ip to 100.96.0.37"));
        assert!(rules(&Policy::read(Mode::Never, None, None)).contains("ipv4 drop"));
        let unfamiliar = br#"{"interface":{},"connector_config":{"routes":[]}}"#;
        assert!(!Policy::read(Mode::Auto, Some(unfamiliar), None).configuration_valid);
    }
}
