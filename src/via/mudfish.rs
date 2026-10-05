// Selects Mudfish nodes by the fields of their location names.
use crate::http::Request;
use crate::text::normalize;
use anyhow::{Context, Result};
use std::fmt;
use std::str::FromStr;

const NODES_URL: &str = "https://mudfish.net/api/staticnodes";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Country,
    Region,
    City,
    Provider,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Term {
    Anywhere(String),
    Field(Field, String),
    Id(u32),
}

impl Term {
    fn parse(text: &str) -> Result<Self, String> {
        let (field, value) = match text.split_once('=') {
            None => return Self::word(text).map(Self::Anywhere),
            Some(split) => split,
        };
        let field = match field {
            "id" => {
                return value
                    .parse()
                    .map(Self::Id)
                    .map_err(|_| format!("invalid node id {value:?}"));
            }
            "country" => Field::Country,
            "region" => Field::Region,
            "city" => Field::City,
            "provider" => Field::Provider,
            _ => {
                return Err(format!(
                    "unknown filter field {field:?}; use country, region, city, provider, or id"
                ));
            }
        };
        Ok(Self::Field(field, Self::word(value)?))
    }

    fn word(text: &str) -> Result<String, String> {
        if text.is_empty()
            || !text
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        {
            return Err(format!("filter term {text:?} must be letters and digits"));
        }
        Ok(text.to_owned())
    }

    pub fn matches(&self, node: &Node) -> bool {
        let place = &node.location;
        match self {
            Self::Anywhere(text) => normalize(&place.text).contains(text.as_str()),
            Self::Field(Field::Country, code) => normalize(&place.country) == *code,
            Self::Field(field, text) => {
                let value = match field {
                    Field::Region => &place.region,
                    Field::City => &place.city,
                    _ => &place.provider,
                };
                normalize(value).contains(text.as_str())
            }
            Self::Id(id) => node.id == *id,
        }
    }
}

// Every positive term must match, and no negative term may match.
// + joins positive terms with AND; & is an alias. A leading - selects exclusions only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filter {
    source: String,
    include: Vec<Term>,
    exclude: Vec<Term>,
}

impl Filter {
    pub fn matches(&self, node: &Node) -> bool {
        self.include.iter().all(|term| term.matches(node))
            && !self.exclude.iter().any(|term| term.matches(node))
    }
}

impl FromStr for Filter {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let lowered = value.to_ascii_lowercase();
        let mut filter = Self {
            source: value.to_owned(),
            include: Vec::new(),
            exclude: Vec::new(),
        };
        // A leading term without a sign counts as +. Delimiters set the next term's sign.
        let mut sign = '+';
        let mut start = 0;
        for (offset, delimiter) in lowered
            .char_indices()
            .filter(|(_, character)| matches!(character, '+' | '-' | '&'))
            .chain(std::iter::once((lowered.len(), '+')))
        {
            let term = &lowered[start..offset];
            if term.is_empty() {
                if offset != 0 || delimiter == '&' || lowered.is_empty() {
                    return Err("empty Mudfish filter term".into());
                }
            } else {
                let parsed = Term::parse(term)?;
                if sign == '-' {
                    &mut filter.exclude
                } else {
                    &mut filter.include
                }
                .push(parsed);
            }
            sign = delimiter;
            start = offset + 1;
        }
        Ok(filter)
    }
}

impl fmt::Display for Filter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.source)
    }
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct Node {
    pub location: Location,
    pub ip: std::net::IpAddr,
    #[serde(rename = "sid")]
    pub id: u32,
}

// Mudfish locations read "CC Region (City - Provider N)", occasionally with a tag or the number
// after the parenthesis, as in "(Tallinn - VPS2Day 2) - PBE" or "(S. Korea - GNJ IDC) 10".
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(from = "String")]
pub struct Location {
    pub text: String,
    pub country: String,
    pub region: String,
    pub city: String,
    pub provider: String,
}

// Drops a trailing node number, so "Vultr 2" and "Vultr 3" share a provider.
fn without_number(text: &str) -> &str {
    match text.rsplit_once(' ') {
        Some((head, tail)) if tail.bytes().all(|byte| byte.is_ascii_digit()) => head.trim(),
        _ => text.trim(),
    }
}

impl From<String> for Location {
    fn from(text: String) -> Self {
        let mut location = Self::default();
        let rest = match text.split_once(' ') {
            Some((code, rest))
                if code.len() == 2 && code.bytes().all(|byte| byte.is_ascii_uppercase()) =>
            {
                location.country = code.to_owned();
                rest
            }
            _ => &text,
        };
        match (rest.find('('), rest.rfind(')')) {
            (Some(open), Some(close)) if open < close => {
                location.region = rest[..open].trim().to_owned();
                let (city, provider) = rest[open + 1..close]
                    .split_once(" - ")
                    .unwrap_or(("", &rest[open + 1..close]));
                location.city = city.trim().to_owned();
                location.provider = without_number(provider).to_owned();
            }
            _ => location.region = rest.trim().to_owned(),
        }
        location.text = text;
        location
    }
}

impl fmt::Display for Location {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.text)
    }
}

pub fn nodes() -> Result<Vec<Node>> {
    #[derive(serde::Deserialize)]
    struct Nodes {
        staticnodes: Vec<Node>,
    }
    let body = Request::new(NODES_URL, 1024 * 1024)
        .get()
        .context("fetching the Mudfish node list")?;
    Ok(serde_json::from_slice::<Nodes>(&body)?.staticnodes)
}

// Failures cluster by provider (credential scope, routing, blocks), so attempts rotate through
// providers in random order, each contributing one random node per round, and cities alternate
// within a provider.
pub fn spread<'a>(nodes: impl IntoIterator<Item = &'a Node>, seed: u64) -> Vec<&'a Node> {
    let mut random = Random(seed | 1);
    let mut providers: Vec<(String, Vec<&Node>)> = Vec::new();
    for node in nodes {
        let provider = normalize(&node.location.provider);
        match providers.iter_mut().find(|(name, _)| *name == provider) {
            Some((_, members)) => members.push(node),
            None => providers.push((provider, vec![node])),
        }
    }
    random.shuffle(&mut providers);
    for (_, members) in &mut providers {
        random.shuffle(members);
        let mut cities: Vec<(String, Vec<&Node>)> = Vec::new();
        for node in members.drain(..) {
            let city = normalize(&node.location.city);
            match cities.iter_mut().find(|(name, _)| *name == city) {
                Some((_, nodes)) => nodes.push(node),
                None => cities.push((city, vec![node])),
            }
        }
        members.extend(round_robin(
            cities.into_iter().map(|(_, nodes)| nodes).collect(),
        ));
    }
    round_robin(providers.into_iter().map(|(_, members)| members).collect())
}

fn round_robin<T>(groups: Vec<Vec<T>>) -> Vec<T> {
    let mut queues: Vec<_> = groups.into_iter().map(Vec::into_iter).collect();
    let mut ordered = Vec::new();
    while !queues.is_empty() {
        queues.retain_mut(|queue| match queue.next() {
            Some(item) => {
                ordered.push(item);
                true
            }
            None => false,
        });
    }
    ordered
}

// xorshift64: shuffling needs variety between runs, not cryptographic strength.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for index in (1..items.len()).rev() {
            items.swap(index, (self.next() % (index as u64 + 1)) as usize);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(location: &str, id: u32) -> Node {
        Node {
            location: location.to_owned().into(),
            ip: "192.0.2.1".parse().unwrap(),
            id,
        }
    }

    #[test]
    fn locations_parse_into_fields_despite_irregular_suffixes() {
        let fields = |text: &str| {
            let location = Location::from(text.to_owned());
            [
                location.country,
                location.region,
                location.city,
                location.provider,
            ]
        };
        assert_eq!(
            fields("US West (Los Angeles - Amazon EC2 2)"),
            ["US", "West", "Los Angeles", "Amazon EC2"]
        );
        assert_eq!(
            fields("EE Europe (Tallinn - VPS2Day 2) - PBE"),
            ["EE", "Europe", "Tallinn", "VPS2Day"]
        );
        assert_eq!(
            fields("KR Asia (S. Korea - GNJ IDC) 10"),
            ["KR", "Asia", "S. Korea", "GNJ IDC"]
        );
    }

    #[test]
    fn spread_rotates_providers_and_alternates_cities() {
        let nodes = [
            node("JP Asia (Osaka - SakuraNet 5)", 1),
            node("JP Asia (Osaka - SakuraNet 6)", 2),
            node("JP Asia (Tokyo - SakuraNet 1)", 3),
            node("JP Asia (Tokyo - Vultr 2)", 4),
            node("JP Asia (Tokyo - Azure 01)", 5),
        ];
        for seed in 1..50 {
            let order = spread(&nodes, seed);
            let providers: Vec<_> = order
                .iter()
                .take(3)
                .map(|node| node.location.provider.as_str())
                .collect();
            assert!(
                providers.contains(&"SakuraNet")
                    && providers.contains(&"Vultr")
                    && providers.contains(&"Azure")
            );
            let sakura: Vec<_> = order
                .iter()
                .filter(|node| node.location.provider == "SakuraNet")
                .collect();
            assert_ne!(sakura[0].location.city, sakura[1].location.city);
        }
    }

    #[test]
    fn filters_match_table_cases() {
        let nodes = [
            node("JP Asia (Osaka - Azure 01)", 1),
            node("JP Asia (Osaka - Azure 02)", 2),
            node("JP Asia (Osaka - Google 1)", 3),
            node("JP Asia (Tokyo - Azure 01)", 4),
            node("JP Asia (Tokyo - Vultr 2)", 5),
            node("SG Asia (Singapore - Azure)", 6),
            node("HK Asia (Hong Kong - Azure 01)", 7),
            node("KR Asia (Seoul - AWS)", 8),
            node("HK Asia (Hong Kong - Vultr)", 9),
        ];
        let cases = [
            ("country=jp+city=osaka+provider=azure", vec![1, 2]),
            ("city=osaka&provider=azure", vec![1, 2]),
            ("country=jp+provider=azure-city=osaka", vec![4]),
            ("-country=jp+provider=azure", vec![6, 7]),
            ("-country=jp-provider=google", vec![6, 7, 8, 9]),
            ("Tokyo", vec![4, 5]),
            ("tokyo+osaka-azure", vec![]),
            ("country=jp+city=tokyo-provider=azure", vec![5]),
            ("-country=jp", vec![6, 7, 8, 9]),
            ("+id=2+city=osaka", vec![2]),
            ("+id=2+seoul", vec![]),
            ("vultr2", vec![5]),
            ("city=hongkong", vec![7, 9]),
            ("country=hk-provider=azure", vec![9]),
            ("country=hk-provider=vultr", vec![7]),
            ("city=osaka+city=singapore", vec![]),
        ];
        for (source, expected) in cases {
            let filter: Filter = source.parse().unwrap();
            let found: Vec<_> = nodes
                .iter()
                .filter(|node| filter.matches(node))
                .map(|node| node.id)
                .collect();
            assert_eq!(found, expected, "filter {source}");
        }
        for source in ["city=Osaka+provider=Azure", "city=Osaka&provider=Azure"] {
            assert_eq!(source.parse::<Filter>().unwrap().to_string(), source);
        }
        let hong_kong = &nodes[6];
        assert!(
            !"country=hk-provider=azure"
                .parse::<Filter>()
                .unwrap()
                .matches(hong_kong)
        );
        // A hyphen starts an exclusion, so the joined spelling is required.
        assert!(
            !"city=hong-kong"
                .parse::<Filter>()
                .unwrap()
                .matches(hong_kong)
        );
    }

    #[test]
    fn invalid_filters_are_rejected() {
        for source in [
            "",
            "+",
            "-",
            "city=osaka+",
            "city=osaka++provider=azure",
            "&",
            "city=osaka&",
            "&provider=azure",
            "city=osaka&&provider=azure",
            "city=osaka&+provider=azure",
            "colo=nrt",
        ] {
            assert!(source.parse::<Filter>().is_err(), "accepted {source}");
        }
    }
}
