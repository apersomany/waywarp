use super::{Subnets, VETH};
use crate::tool;
use crate::warp::LINK;
use anyhow::{Context, Result, bail};
use nix::libc;
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub struct Routing {
    warp_table: u32,
    host_table: u32,
}

#[derive(Deserialize)]
struct Rule {
    priority: u32,
    src: String,
    #[serde(default, deserialize_with = "table_id")]
    table: Option<u32>,
    iif: Option<String>,
    #[serde(flatten)]
    attributes: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct Route {
    #[serde(default, deserialize_with = "table_id")]
    table: Option<u32>,
}

// ip's numeric JSON mode still represents some table IDs as strings.
fn table_id<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u32>, D::Error> {
    let value = Value::deserialize(deserializer)?;
    let number = match &value {
        Value::Number(number) => number.as_u64().and_then(|id| u32::try_from(id).ok()),
        Value::String(name) => name.parse().ok(),
        _ => None,
    };
    number
        .map(Some)
        .ok_or_else(|| serde::de::Error::custom("expected a numeric routing table ID"))
}

impl Rule {
    fn read(family: &str) -> Result<Vec<Self>> {
        Ok(serde_json::from_str(&tool::run(
            "ip",
            &["-j", "-N", family, "rule", "show"],
        )?)?)
    }

    fn lookup(&self, table: u32, ingress: Option<&str>) -> bool {
        self.table == Some(table)
            && self.src == "all"
            && self.iif.as_deref() == ingress
            && self
                .attributes
                .keys()
                .all(|key| matches!(key.as_str(), "protocol" | "iif_detached"))
    }
}

impl Routing {
    pub fn allocate() -> Result<Self> {
        let mut rules = Vec::new();
        let mut routes = Vec::new();
        for family in ["-4", "-6"] {
            rules.extend(Rule::read(family)?);
            routes.extend(serde_json::from_str::<Vec<Route>>(&tool::run(
                "ip",
                &["-j", "-N", family, "route", "show", "table", "all"],
            )?)?);
        }
        Self::unused_tables(&rules, &routes)
    }

    fn unused_tables(rules: &[Rule], routes: &[Route]) -> Result<Self> {
        let mut used: BTreeSet<u32> = [
            libc::RT_TABLE_UNSPEC,
            libc::RT_TABLE_COMPAT,
            libc::RT_TABLE_DEFAULT,
            libc::RT_TABLE_MAIN,
            libc::RT_TABLE_LOCAL,
        ]
        .into_iter()
        .map(u32::from)
        .chain(rules.iter().filter_map(|rule| rule.table))
        .chain(routes.iter().filter_map(|route| route.table))
        .collect();
        let mut next = || -> Result<u32> {
            let table = (1..=u32::MAX)
                .find(|table| !used.contains(table))
                .context("no unused bridge routing table")?;
            used.insert(table);
            Ok(table)
        };
        Ok(Self {
            warp_table: next()?,
            host_table: next()?,
        })
    }

    pub fn install(&self, subnets: Subnets) -> Result<()> {
        for (family, gateway) in [
            ("-4", format!("{}/30", subnets.v4().gateway)),
            ("-6", format!("{}/126", subnets.v6().gateway)),
        ] {
            tool::ip(
                family,
                &format!(
                    "address add {gateway} dev {VETH}\n\
                     route add unreachable default table {} metric 4096",
                    self.warp_table
                ),
            )?;
        }
        tool::ip("-4", &format!("link set {VETH} up"))?;
        for (family, host) in [
            ("-4", subnets.v4().host.to_string()),
            ("-6", subnets.v6().host.to_string()),
        ] {
            tool::ip(
                family,
                &format!(
                    "route add default via {host} dev {VETH} table {}",
                    self.host_table
                ),
            )?;
        }
        self.ensure_rules()
    }

    fn owns(&self, rule: &Rule) -> bool {
        rule.lookup(self.host_table, Some(LINK)) || rule.lookup(self.warp_table, Some(VETH))
    }

    fn rule_commands(&self, rules: &[Rule]) -> Result<String> {
        // Never overwrite a table that another policy has started using since allocation.
        for rule in rules {
            if matches!(rule.table, Some(table) if table == self.host_table || table == self.warp_table)
                && !self.owns(rule)
            {
                bail!(
                    "bridge routing table {} is used by another policy",
                    rule.table.unwrap()
                );
            }
        }
        let foreign: Vec<_> = rules.iter().filter(|rule| !self.owns(rule)).collect();
        let local = foreign
            .iter()
            .filter(|rule| rule.lookup(u32::from(libc::RT_TABLE_LOCAL), None))
            .map(|rule| rule.priority)
            .min()
            .context("missing unconditional local routing rule")?;
        let main = foreign
            .iter()
            .filter(|rule| rule.lookup(u32::from(libc::RT_TABLE_MAIN), None))
            .map(|rule| rule.priority)
            .min()
            .context("missing unconditional main routing rule")?;
        let host = local
            .checked_add(1)
            .context("no priority after the local rule")?;
        let warp = main
            .checked_sub(1)
            .context("no priority before the main rule")?;
        if host >= warp
            || foreign.iter().any(|rule| {
                !rule.lookup(u32::from(libc::RT_TABLE_LOCAL), None) && rule.priority <= host
            })
        {
            bail!("no bridge return-rule priority between local lookup and other policies");
        }
        if foreign.iter().any(|rule| rule.priority == warp) {
            bail!("no bridge fallback-rule priority immediately before main lookup");
        }

        let mut commands = String::new();
        // Return traffic must beat WARP's managed routes, but local connector addresses must
        // still reach local services. Only ingress matches; WARP's own sockets are unaffected.
        // The outbound fallback remains after WARP's policies and before the physical uplink.
        for (ingress, table, priority) in
            [(LINK, self.host_table, host), (VETH, self.warp_table, warp)]
        {
            let existing: Vec<_> = rules
                .iter()
                .filter(|rule| rule.lookup(table, Some(ingress)))
                .collect();
            if !existing.iter().any(|rule| rule.priority == priority) {
                commands.push_str(&format!(
                    "rule add pref {priority} iif {ingress} lookup {table}\n"
                ));
            }
            let mut retained = false;
            for rule in existing {
                if rule.priority == priority && !retained {
                    retained = true;
                } else {
                    commands.push_str(&format!(
                        "rule del pref {} iif {ingress} lookup {table}\n",
                        rule.priority
                    ));
                }
            }
        }
        Ok(commands)
    }

    fn ensure_rules(&self) -> Result<()> {
        for family in ["-4", "-6"] {
            let commands = self.rule_commands(&Rule::read(family)?)?;
            if !commands.is_empty() {
                tool::ip(family, &commands)?;
            }
        }
        Ok(())
    }

    pub fn reconcile(&self) -> Result<()> {
        self.ensure_rules()?;
        // The kernel reattaches name-based ingress rules when WARP recreates its link, and
        // the host-return route uses veth. Only the route into WARP needs reinstalling.
        for family in ["-4", "-6"] {
            tool::ip(
                family,
                &format!("route replace default dev {LINK} table {}", self.warp_table),
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rules(extra: Value) -> Vec<Rule> {
        let mut rules = json!([
            {"priority": 0, "src": "all", "table": "255"},
            {"priority": 32766, "src": "all", "table": 254}
        ]);
        rules
            .as_array_mut()
            .unwrap()
            .extend(extra.as_array().unwrap().clone());
        serde_json::from_value(rules).unwrap()
    }

    fn routing() -> Routing {
        Routing {
            warp_table: 1,
            host_table: 2,
        }
    }

    #[test]
    fn tables_consider_routes_and_rules_from_both_families() {
        let rules = rules(json!([
            {"priority": 99, "src": "all", "table": "1"},
            {"priority": 42, "src": "all", "table": 3}
        ]));
        let routes: Vec<Route> = serde_json::from_value(json!([
            {"table": "2"}, {"table": 4}, {"dst": "default"}
        ]))
        .unwrap();
        let routing = Routing::unused_tables(&rules, &routes).unwrap();
        assert_eq!((routing.warp_table, routing.host_table), (5, 6));
        let routes: Vec<Route> = (1..252).map(|table| Route { table: Some(table) }).collect();
        let routing = Routing::unused_tables(&[], &routes).unwrap();
        assert_eq!((routing.warp_table, routing.host_table), (256, 257));
    }

    #[test]
    fn table_ids_must_be_numeric_and_fit_in_u32() {
        for table in [json!("65743"), json!(65743)] {
            let route: Route = serde_json::from_value(json!({"table": table})).unwrap();
            assert_eq!(route.table, Some(65743));
        }
        for table in [json!("warp"), json!(-1), json!(4294967296_u64), Value::Null] {
            assert!(serde_json::from_value::<Route>(json!({"table": table})).is_err());
        }
    }

    #[test]
    fn priorities_follow_local_and_main_not_warp_constants() {
        for priority in [2, 42, 99, 100, 32000] {
            let rules = rules(json!([
                {"priority": priority, "src": "all", "not": true, "fwmark": "0x100cf", "table": "65743"}
            ]));
            assert_eq!(
                routing().rule_commands(&rules).unwrap(),
                "rule add pref 1 iif CloudflareWARP lookup 2\nrule add pref 32765 iif veth lookup 1\n"
            );
        }
        let rules: Vec<Rule> = serde_json::from_value(json!([
            {"priority": 7, "src": "all", "table": 255},
            {"priority": 42, "src": "all", "table": 65743},
            {"priority": 1024, "src": "all", "table": 254}
        ]))
        .unwrap();
        assert_eq!(
            routing().rule_commands(&rules).unwrap(),
            "rule add pref 8 iif CloudflareWARP lookup 2\nrule add pref 1023 iif veth lookup 1\n"
        );
    }

    #[test]
    fn rule_reconciliation_is_idempotent_and_repairs_missing_or_duplicate_rules() {
        let installed = rules(json!([
            {"priority": 1, "src": "all", "iif": "CloudflareWARP", "iif_detached": null, "table": 2},
            {"priority": 32765, "src": "all", "iif": "veth", "table": 1}
        ]));
        assert!(routing().rule_commands(&installed).unwrap().is_empty());
        let outdated = rules(json!([
            {"priority": 101, "src": "all", "iif": "CloudflareWARP", "table": 2},
            {"priority": 32765, "src": "all", "iif": "veth", "table": 1},
            {"priority": 32765, "src": "all", "iif": "veth", "table": 1}
        ]));
        assert_eq!(
            routing().rule_commands(&outdated).unwrap(),
            "rule add pref 1 iif CloudflareWARP lookup 2\nrule del pref 101 iif CloudflareWARP lookup 2\nrule del pref 32765 iif veth lookup 1\n"
        );
    }

    #[test]
    #[ignore = "requires network namespace capabilities, iproute2 and nftables"]
    fn kernel_return_routing_survives_managed_routes_and_link_recreation() {
        let private = crate::sandbox::Private::create().unwrap();
        private.run(|| {
            tool::ip("-4", &format!(
                "link set lo up\nlink add {VETH} type dummy\nlink add tun type dummy\nlink set tun up\nroute add default dev tun\nrule add pref 200 lookup 1"
            ))?;
            tool::ip("-6", "route add unreachable 2001:db8:cc::/64 table 2")?;
            let routing = super::super::firewall(Subnets::new(0, None, None))?;
            assert_eq!((routing.warp_table, routing.host_table), (3, 4));
            std::fs::write("/proc/sys/net/ipv4/conf/all/rp_filter", "0")?;
            let mut previous = None;
            for priority in [99, 42, 2] {
                if let Some(previous) = previous {
                    tool::ip("-4", &format!("link del {LINK}"))?;
                    for family in ["-4", "-6"] {
                        tool::ip(family, &format!("rule del pref {previous} lookup 65743"))?;
                    }
                }
                tool::ip("-4", &format!(
                    "link add {LINK} type dummy\nlink set {LINK} up\naddress add 172.16.0.2/32 dev {LINK}"
                ))?;
                tool::ip("-6", &format!(
                    "address add 2001:db8::2/128 dev {LINK} nodad\naddress add 2001:db8::1/128 dev {LINK} nodad"
                ))?;
                std::fs::write(format!("/proc/sys/net/ipv4/conf/{LINK}/rp_filter"), "0")?;
                for (family, subnet) in [
                    ("-4", "100.96.0.0/12"),
                    ("-6", "2606:4700:cf1:1000::/64"),
                ] {
                    tool::ip(family, &format!(
                        "route add {subnet} dev {LINK} table 65743\nrule add pref {priority} not fwmark 0x100cf lookup 65743"
                    ))?;
                }
                routing.reconcile()?;
                for (family, destination, source, local) in [
                    ("-4", "100.96.0.42", "203.0.113.1", "172.16.0.2"),
                    ("-6", "2606:4700:cf1:1000::42", "2001:db8:1::443", "2001:db8::1"),
                ] {
                    let lookup = |destination: &str, forwarded: bool| -> Result<Value> {
                        let mut arguments = vec!["-j", family, "route", "get", destination];
                        if forwarded {
                            arguments.extend(["from", source, "iif", LINK]);
                        }
                        let routes: Vec<Value> = serde_json::from_str(&tool::run("ip", &arguments)?)?;
                        Ok(routes.into_iter().next().unwrap())
                    };
                    let reply = lookup(destination, true)?;
                    assert_eq!(reply["dev"], VETH, "{reply}");
                    assert_eq!(reply["table"], routing.host_table.to_string(), "{reply}");
                    let control = lookup(destination, false)?;
                    assert_eq!(control["dev"], LINK, "{control}");
                    assert_eq!(control["table"], "65743", "{control}");
                    let local = lookup(local, true)?;
                    assert_eq!(local["type"], "local", "{local}");
                    assert_eq!(local["table"], "local", "{local}");
                    let rules = Rule::read(family)?;
                    assert_eq!(rules.iter().filter(|rule| routing.owns(rule)).count(), 2);
                    assert!(routing.rule_commands(&rules)?.is_empty());
                }
                previous = Some(priority);
            }
            Ok(())
        }).unwrap();
    }

    #[test]
    fn unsafe_priorities_and_table_collisions_fail_instead_of_changing_other_policies() {
        for extra in [
            json!([{"priority": 1, "src": "all", "table": 65743}]),
            json!([{"priority": 0, "src": "all", "table": 65743}]),
            json!([{"priority": 32765, "src": "all", "table": 65743}]),
            json!([{"priority": 99, "src": "all", "table": 2}]),
            json!([{"priority": 99, "src": "all", "iif": "CloudflareWARP", "fwmark": "1", "table": 2}]),
        ] {
            assert!(routing().rule_commands(&rules(extra)).is_err());
        }
        assert!(routing().rule_commands(&[]).is_err());
        let conditional: Vec<Rule> = serde_json::from_value(json!([
            {"priority": 0, "src": "192.0.2.0/24", "table": 255},
            {"priority": 32766, "src": "all", "table": 254}
        ]))
        .unwrap();
        assert!(routing().rule_commands(&conditional).is_err());
    }
}
