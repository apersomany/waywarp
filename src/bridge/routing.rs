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
        let is_local = |rule: &Rule| rule.lookup(u32::from(libc::RT_TABLE_LOCAL), None);
        let foreign: Vec<_> = rules.iter().filter(|rule| !self.owns(rule)).collect();
        let local = foreign
            .iter()
            .filter(|rule| is_local(rule))
            .map(|rule| rule.priority)
            .min()
            .context("missing unconditional local routing rule")?;
        let main = foreign
            .iter()
            .filter(|rule| rule.lookup(u32::from(libc::RT_TABLE_MAIN), None))
            .map(|rule| rule.priority)
            .min()
            .context("missing unconditional main routing rule")?;
        let warp = main
            .checked_sub(1)
            .context("no priority before the main rule")?;
        if foreign.iter().any(|rule| rule.priority == warp) {
            bail!("no bridge fallback-rule priority immediately before main lookup");
        }

        // Rules at one priority apply in the order they were added, and the kernel gives a
        // policy added without a priority the same priority as the rule after local delivery
        // when that is 0. Return routing installed there before WARP starts therefore stays
        // ahead of WARP's rule however often it is recreated. Foreign rules are never moved,
        // so another policy ahead of that position is a conflict.
        let local_at = rules
            .iter()
            .position(|rule| is_local(rule) && !self.owns(rule) && rule.priority == local)
            .unwrap();
        let host = rules
            .iter()
            .rposition(|rule| rule.lookup(self.host_table, Some(LINK)) && rule.priority == local)
            .filter(|index| *index > local_at);
        let ahead = host.unwrap_or_else(|| rules.partition_point(|rule| rule.priority <= local));
        if !rules[..ahead]
            .iter()
            .all(|rule| self.owns(rule) || is_local(rule))
        {
            bail!("another policy precedes the bridge return rule at priority {local}");
        }
        let warp_at = rules
            .iter()
            .rposition(|rule| rule.lookup(self.warp_table, Some(VETH)) && rule.priority == warp);

        let mut commands = String::new();
        // Return traffic must beat WARP's managed routes, but local connector addresses must
        // still reach local services. Only ingress matches; WARP's own sockets are unaffected.
        // The outbound fallback remains after WARP's policies and before the physical uplink.
        // Deleting removes the earliest match, so the last of several equal copies is kept.
        for (ingress, table, priority, keep) in [
            (LINK, self.host_table, local, host),
            (VETH, self.warp_table, warp, warp_at),
        ] {
            if keep.is_none() {
                commands.push_str(&format!(
                    "rule add pref {priority} iif {ingress} lookup {table}\n"
                ));
            }
            for (index, rule) in rules.iter().enumerate() {
                if rule.lookup(table, Some(ingress)) && Some(index) != keep {
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

    // Kernel order: by priority, with later additions after earlier ones.
    fn rules(extra: Value) -> Vec<Rule> {
        let mut rules: Vec<Rule> = serde_json::from_value(json!([
            {"priority": 0, "src": "all", "table": "255"},
            {"priority": 32766, "src": "all", "table": 254}
        ]))
        .unwrap();
        for rule in extra.as_array().unwrap() {
            insert(&mut rules, rule.clone());
        }
        rules
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

    fn warp(priority: u32) -> Value {
        json!({"priority": priority, "src": "all", "not": true, "fwmark": "0x100cf", "table": 65743})
    }

    fn owned(priority: u32, ingress: &str, table: u32) -> Value {
        json!({"priority": priority, "src": "all", "iif": ingress, "table": table})
    }

    // Rules are kept in evaluation order; the kernel appends a rule after its equals.
    fn insert(rules: &mut Vec<Rule>, rule: Value) {
        let rule: Rule = serde_json::from_value(rule).unwrap();
        let index = rules.partition_point(|other| other.priority <= rule.priority);
        rules.insert(index, rule);
    }

    // A policy added without a priority sits just before the second rule, or at 0 beside it.
    fn connect_warp(rules: &mut Vec<Rule>) {
        let priority = rules[1].priority.saturating_sub(1);
        insert(rules, warp(priority));
    }

    fn disconnect_warp(rules: &mut Vec<Rule>) {
        rules.retain(|rule| rule.table != Some(65743));
    }

    fn apply(commands: &str, rules: &mut Vec<Rule>) {
        for command in commands.lines() {
            let words: Vec<_> = command.split(' ').collect();
            let priority: u32 = words[3].parse().unwrap();
            let (ingress, table) = (words[5], words[7].parse().unwrap());
            if words[1] == "add" {
                insert(rules, owned(priority, ingress, table));
            } else {
                let index = rules
                    .iter()
                    .position(|rule| rule.priority == priority && rule.lookup(table, Some(ingress)))
                    .unwrap();
                rules.remove(index);
            }
        }
    }

    fn reconcile(routing: &Routing, rules: &mut Vec<Rule>) {
        apply(&routing.rule_commands(rules).unwrap(), rules);
        assert!(routing.rule_commands(rules).unwrap().is_empty());
    }

    fn order(rules: &[Rule]) -> Vec<(u32, Option<String>, Option<u32>)> {
        rules
            .iter()
            .map(|rule| (rule.priority, rule.iif.clone(), rule.table))
            .collect()
    }

    #[test]
    fn return_rule_follows_local_delivery_and_fallback_precedes_main() {
        let rules = rules(json!([warp(99)]));
        let routing = routing();
        assert_eq!(
            routing.rule_commands(&rules).unwrap(),
            "rule add pref 0 iif CloudflareWARP lookup 2\nrule add pref 32765 iif veth lookup 1\n"
        );
        let moved: Vec<Rule> = serde_json::from_value(json!([
            {"priority": 7, "src": "all", "table": 255},
            {"priority": 42, "src": "all", "table": 65743},
            {"priority": 1024, "src": "all", "table": 254},
            {"priority": 32767, "src": "all", "table": 253}
        ]))
        .unwrap();
        assert_eq!(
            routing.rule_commands(&moved).unwrap(),
            "rule add pref 7 iif CloudflareWARP lookup 2\nrule add pref 1023 iif veth lookup 1\n"
        );
    }

    #[test]
    fn rule_reconciliation_is_idempotent_and_repairs_missing_or_duplicate_rules() {
        let mut installed = rules(json!([
            owned(0, "CloudflareWARP", 2),
            owned(32765, "veth", 1),
            warp(0),
            warp(99)
        ]));
        assert!(routing().rule_commands(&installed).unwrap().is_empty());
        installed.retain(|rule| rule.table != Some(1));
        assert_eq!(
            routing().rule_commands(&installed).unwrap(),
            "rule add pref 32765 iif veth lookup 1\n"
        );
        let outdated = rules(json!([
            owned(101, "CloudflareWARP", 2),
            owned(0, "CloudflareWARP", 2),
            owned(0, "CloudflareWARP", 2),
            owned(32765, "veth", 1),
            owned(32765, "veth", 1)
        ]));
        assert_eq!(
            routing().rule_commands(&outdated).unwrap(),
            "rule del pref 0 iif CloudflareWARP lookup 2\nrule del pref 101 iif CloudflareWARP lookup 2\nrule del pref 32765 iif veth lookup 1\n"
        );
        let missing = rules(json!([
            owned(101, "CloudflareWARP", 2),
            owned(32765, "veth", 1)
        ]));
        assert_eq!(
            routing().rule_commands(&missing).unwrap(),
            "rule add pref 0 iif CloudflareWARP lookup 2\nrule del pref 101 iif CloudflareWARP lookup 2\n"
        );
    }

    #[test]
    fn duplicate_cleanup_keeps_the_copy_that_deletion_leaves_behind() {
        let routing = routing();
        let copy = |ingress: &str, table: u32, priority: u32, protocol: u32| json!({"priority": priority, "src": "all", "iif": ingress, "table": table, "protocol": protocol});
        // Deleting by selector removes the earliest copy, so the last one must be the safe one.
        let mut safe = rules(json!([
            copy("CloudflareWARP", 2, 0, 42),
            copy("CloudflareWARP", 2, 0, 43),
            warp(0),
            copy("veth", 1, 32765, 7),
            copy("veth", 1, 32765, 8)
        ]));
        reconcile(&routing, &mut safe);
        let kept: Vec<_> = safe.iter().filter(|rule| routing.owns(rule)).collect();
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].attributes["protocol"], 43);
        assert_eq!(kept[1].attributes["protocol"], 8);
        assert!(safe[1].lookup(2, Some(LINK)) && safe[2].table == Some(65743));
        // A copy behind a foreign policy cannot be told apart from the early one by deleting.
        let split = rules(json!([
            copy("CloudflareWARP", 2, 0, 42),
            warp(0),
            copy("CloudflareWARP", 2, 0, 43)
        ]));
        assert!(routing.rule_commands(&split).is_err());
    }

    #[test]
    fn a_policy_ahead_of_the_return_slot_is_a_conflict_that_changes_nothing() {
        // Equal priorities apply in insertion order, so nothing can be placed ahead of these.
        let conflict = rules(json!([warp(0)]));
        assert!(routing().rule_commands(&conflict).is_err());
        let late = rules_with_return_behind(json!([warp(0)]));
        assert!(routing().rule_commands(&late).is_err());
        // The same rule at another priority leaves the slot free.
        assert!(routing().rule_commands(&rules(json!([warp(1)]))).is_ok());
    }

    // Our rule added after a foreign one at the same priority, as in a namespace that was
    // populated before Waywarp.
    fn rules_with_return_behind(extra: Value) -> Vec<Rule> {
        let mut all = rules(extra);
        insert(&mut all, owned(0, "CloudflareWARP", 2));
        all
    }

    #[test]
    fn return_rule_added_after_local_delivery_stays_ahead_of_recreated_warp_rules() {
        let routing = routing();
        for persistent in [json!([]), json!([unrelated(50), unrelated(0)])] {
            let mut rules = self::rules(json!([]));
            reconcile(&routing, &mut rules);
            for policy in persistent.as_array().unwrap() {
                insert(&mut rules, policy.clone());
            }
            let start = rules.len();
            connect_warp(&mut rules);
            reconcile(&routing, &mut rules);
            let expected = order(&rules);
            // Recreating WARP's rule many times without any reconcile in between must neither
            // reorder nor renumber anything; the cycle count dwarfs any priority budget.
            for _ in 0..50_000 {
                disconnect_warp(&mut rules);
                connect_warp(&mut rules);
                connect_warp(&mut rules);
                disconnect_warp(&mut rules);
                connect_warp(&mut rules);
                assert_eq!(rules.len(), start + 1);
            }
            assert_eq!(order(&rules), expected);
            assert!(routing.rule_commands(&rules).unwrap().is_empty());
            let host = rules
                .iter()
                .position(|rule| rule.lookup(2, Some(LINK)))
                .unwrap();
            let managed = rules
                .iter()
                .position(|rule| rule.table == Some(65743))
                .unwrap();
            assert!(1 == host && host < managed, "{expected:?}");
        }
    }

    fn unrelated(priority: u32) -> Value {
        json!({"priority": priority, "src": "all", "oif": "lo", "table": 7})
    }

    #[test]
    #[ignore = "requires network namespace capabilities, iproute2 and nftables"]
    fn kernel_return_routing_stays_ahead_of_recreated_warp_rules() {
        let private = crate::sandbox::Private::create().unwrap();
        private.run(|| {
            tool::ip("-4", &format!(
                "link set lo up\nlink add {VETH} type dummy\nlink add tun type dummy\nlink set tun up\nroute add default dev tun\nrule add pref 200 lookup 1"
            ))?;
            tool::ip("-6", "route add unreachable 2001:db8:cc::/64 table 2")?;
            let layout = || -> Result<Vec<_>> {
                let mut layout = Vec::new();
                for family in ["-4", "-6"] {
                    for rule in Rule::read(family)? {
                        layout.push((family, rule.priority, rule.iif, rule.table));
                    }
                }
                Ok(layout)
            };
            // A policy already at the local priority cannot be moved behind ours: nothing changes.
            let candidate = Routing::allocate()?;
            for family in ["-4", "-6"] {
                tool::ip(family, "rule add pref 0 not fwmark 0x100cf lookup 65743")?;
            }
            let before = layout()?;
            assert!(candidate.ensure_rules().is_err());
            assert_eq!(layout()?, before);
            for family in ["-4", "-6"] {
                tool::ip(family, "rule del pref 0 not fwmark 0x100cf lookup 65743")?;
            }

            let routing = super::super::firewall(Subnets::new(0, None, None))?;
            assert_eq!((routing.warp_table, routing.host_table), (3, 4));
            // Copies that differ only in protocol are separate rules, and cleanup must leave the
            // last one, which is the copy that still precedes everything added later.
            for family in ["-4", "-6"] {
                tool::ip(family, &format!(
                    "rule add pref 0 iif {LINK} lookup {0} protocol 42\nrule add pref 0 iif {LINK} lookup {0} protocol 43",
                    routing.host_table
                ))?;
            }
            // Unrelated policies persist, including one that is added later at the same priority.
            for family in ["-4", "-6"] {
                tool::ip(family, "rule add pref 0 oif lo lookup 77")?;
            }
            std::fs::write("/proc/sys/net/ipv4/conf/all/rp_filter", "0")?;
            let mut first = None;
            for _ in 0..50 {
                // WARP recreates its link and rule several times before anything reconciles.
                for _ in 0..3 {
                    tool::run("ip", &["link", "del", LINK]).ok();
                    for family in ["-4", "-6"] {
                        tool::run("ip", &[family, "rule", "del", "not", "fwmark", "0x100cf", "lookup", "65743"]).ok();
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
                        // Like warp-svc, request no priority and leave the choice to the kernel.
                        tool::ip(family, &format!(
                            "route add {subnet} dev {LINK} table 65743\nrule add not fwmark 0x100cf lookup 65743"
                        ))?;
                    }
                }
                routing.reconcile()?;
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
                    // Local delivery, then bridge return, both at the local priority.
                    assert!(rules[0].lookup(255, None) && rules[1].lookup(routing.host_table, Some(LINK)));
                    assert_eq!(rules[1].priority, 0);
                }
                assert_eq!(*first.get_or_insert(layout()?), layout()?);
            }
            // A copy added behind WARP's rule is refused, because deleting could only remove the
            // earlier one. Nothing changes until it is gone.
            let settled = layout()?;
            tool::ip("-4", &format!(
                "rule add pref 0 iif {LINK} lookup {} protocol 44", routing.host_table
            ))?;
            let stray = layout()?;
            assert!(routing.reconcile().is_err());
            assert_eq!(layout()?, stray);
            tool::ip("-4", &format!(
                "rule del pref 0 iif {LINK} lookup {} protocol 44", routing.host_table
            ))?;
            routing.reconcile()?;
            assert_eq!(layout()?, settled);
            Ok(())
        }).unwrap();
    }

    #[test]
    fn unsafe_priorities_and_table_collisions_fail_instead_of_changing_other_policies() {
        for extra in [
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
