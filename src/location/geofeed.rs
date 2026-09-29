// Cloudflare's colo catalog and IP geofeed, cached for a day and shared by every instance in a
// store.
use super::Place;
use crate::http::Request;
use crate::store::Store;
use crate::text::normalize;
use anyhow::{Result, bail};
use ipnet::IpNet;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::net::IpAddr;
use std::path::Path;
use std::time::{Duration, SystemTime};
use tracing::{debug, warn};

const COLOS_URL: &str = "https://speed.cloudflare.com/locations";
const GEOFEED_URL: &str = "https://api.cloudflare.com/local-ip-ranges.csv";
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Deserialize)]
struct Colo {
    iata: String,
    cca2: String,
}

pub struct Geofeed {
    // Colo code to country code.
    colos: HashMap<String, String>,
    // Most specific first, so the first containing range wins.
    ranges: Vec<(IpNet, Place)>,
}

fn fresh(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < MAX_AGE)
}

fn update(path: &Path, url: &str, headers: &[(&str, &str)], limit: u64) -> Result<()> {
    debug!(url, "refreshing");
    let data = Request {
        headers,
        ..Request::new(url, limit)
    }
    .get()?;
    // Every instance in a store shares the cache, so each writes its own temporary file.
    let temporary = path.with_extension(format!("{}.new", std::process::id()));
    fs::write(&temporary, data)?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn ensure(path: &Path, url: &str, headers: &[(&str, &str)], limit: u64) -> Result<()> {
    if fresh(path) {
        return Ok(());
    }
    match update(path, url, headers, limit) {
        Ok(()) => Ok(()),
        Err(error) if path.is_file() => {
            warn!(path = %path.display(), "using stale data: {error:#}");
            Ok(())
        }
        Err(error) => Err(error),
    }
}

impl Geofeed {
    pub fn load(store: &Store) -> Result<Self> {
        let colos = store.cache("locations.json");
        let geofeed = store.cache("local-ip-ranges.csv");
        // The colo catalog only answers requests that appear to come from the speed test page.
        ensure(
            &colos,
            COLOS_URL,
            &[("Origin", "https://speed.cloudflare.com")],
            128 * 1024,
        )?;
        ensure(&geofeed, GEOFEED_URL, &[], 8 * 1024 * 1024)?;
        Self::parse(&fs::read(colos)?, &fs::read_to_string(geofeed)?)
    }

    fn parse(colos: &[u8], geofeed: &str) -> Result<Self> {
        let colos: Vec<Colo> = serde_json::from_slice(colos)?;
        let colos: HashMap<_, _> = colos
            .into_iter()
            .map(|colo| {
                (
                    colo.iata.to_ascii_uppercase(),
                    colo.cca2.to_ascii_uppercase(),
                )
            })
            .collect();
        // Rows read `network,country,region,city,postal`.
        let mut ranges: Vec<_> = geofeed
            .lines()
            .filter_map(|line| {
                let mut fields = line.split(',');
                let network: IpNet = fields.next()?.parse().ok()?;
                let country = fields.next()?;
                let city = fields.nth(1)?.trim();
                (country.len() == 2).then(|| {
                    let place = Place {
                        country: country.to_ascii_uppercase(),
                        city: (!city.is_empty()).then(|| city.to_owned()),
                    };
                    (network.trunc(), place)
                })
            })
            .collect();
        ranges.sort_by_key(|(network, _)| {
            std::cmp::Reverse((network.addr().is_ipv6(), network.prefix_len()))
        });
        let family = |ipv6: bool| {
            ranges
                .iter()
                .any(|(network, _)| network.addr().is_ipv6() == ipv6)
        };
        if colos.is_empty() || !family(false) || !family(true) {
            bail!("Cloudflare returned incomplete location data");
        }
        Ok(Self { colos, ranges })
    }

    pub fn place(&self, address: IpAddr) -> Option<&Place> {
        let found = self
            .ranges
            .iter()
            .find(|(network, _)| network.contains(&address))
            .map(|(_, place)| place);
        if found.is_none() {
            warn!(%address, "Cloudflare's geofeed does not contain the address");
        }
        found
    }

    pub fn knows_colo(&self, code: &str) -> bool {
        self.colos.contains_key(code)
    }

    pub fn colo_country(&self, code: &str) -> Option<&str> {
        self.colos.get(code).map(String::as_str)
    }

    pub fn knows(&self, wanted: &Place) -> bool {
        let city = wanted.city.as_deref().map(normalize);
        self.ranges.iter().any(|(_, place)| {
            place.country == wanted.country
                && city
                    .as_ref()
                    .is_none_or(|city| place.city.as_deref().map(normalize).as_ref() == Some(city))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_most_specific_range_wins() {
        let geofeed = Geofeed::parse(
            br#"[{"iata":"NRT","cca2":"JP"}]"#,
            "104.28.0.0/16,US,,,\n104.28.136.27/32,JP,JP-12,Narita,\n2a09:bac1::/32,JP,,Tokyo,\n",
        )
        .unwrap();
        let place = |address: &str| geofeed.place(address.parse().unwrap()).cloned();
        assert_eq!(place("104.28.136.27"), Some("JP/Narita".parse().unwrap()));
        assert_eq!(place("104.28.1.1"), Some("US".parse().unwrap()));
        assert_eq!(place("192.0.2.1"), None);
    }
}
