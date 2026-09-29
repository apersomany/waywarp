// Where a connected tunnel sits, and the `--location` constraints it must satisfy. See
// docs/locations.md.
pub mod geofeed;
pub mod probe;

use crate::text::normalize;
use anyhow::{Result, bail};
use geofeed::Geofeed;
use probe::Probe;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

// A country code, optionally with a city as Cloudflare's geofeed spells it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Place {
    pub country: String,
    pub city: Option<String>,
}

impl Place {
    // A place without a city matches every city in its country; cities compare like Mudfish
    // filters, ignoring case, spaces, and punctuation.
    fn contains(&self, other: &Self) -> bool {
        self.country == other.country
            && self.city.as_ref().is_none_or(|city| {
                other
                    .city
                    .as_ref()
                    .is_some_and(|other| normalize(city) == normalize(other))
            })
    }
}

impl FromStr for Place {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (country, city) = match value.split_once('/') {
            Some((country, city)) => (country, Some(city.trim())),
            None => (value, None),
        };
        if country.len() != 2 || !country.bytes().all(|byte| byte.is_ascii_alphabetic()) {
            return Err(format!(
                "{value:?} is not a place; use a country code such as JP, optionally with a city as in JP/Tokyo"
            ));
        }
        if city.is_some_and(|city| normalize(city).is_empty()) {
            return Err(format!("{value:?} names no city after the slash"));
        }
        Ok(Self {
            country: country.to_ascii_uppercase(),
            city: city.map(str::to_owned),
        })
    }
}

impl fmt::Display for Place {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.city {
            Some(city) => write!(formatter, "{}/{city}", self.country),
            None => formatter.write_str(&self.country),
        }
    }
}

fn colo(value: &str) -> Result<String, String> {
    if value.len() != 3 || !value.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return Err(format!(
            "{value:?} is not a colo; use a three-letter code such as NRT"
        ));
    }
    Ok(value.to_ascii_uppercase())
}

// Every field that is set must match; fields are named after the status fields they check.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Constraints {
    pub geo4: Option<Place>,
    pub geo6: Option<Place>,
    pub edge: Option<String>,
    pub probe4: Option<String>,
    pub probe6: Option<String>,
}

impl FromStr for Constraints {
    type Err = String;

    // `field=VALUE` terms joined by `+`, as in geo4=JP/Tokyo+edge=NRT; case does not matter.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut constraints = Self::default();
        for term in value.split('+') {
            let (field, value) = term.split_once('=').ok_or_else(|| {
                format!("{term:?} is not a constraint; use FIELD=VALUE, as in geo4=JP")
            })?;
            let field = field.to_ascii_lowercase();
            let repeated = match field.as_str() {
                "geo4" => constraints.geo4.replace(value.parse()?).is_some(),
                "geo6" => constraints.geo6.replace(value.parse()?).is_some(),
                "edge" => constraints.edge.replace(colo(value)?).is_some(),
                "probe4" => constraints.probe4.replace(colo(value)?).is_some(),
                "probe6" => constraints.probe6.replace(colo(value)?).is_some(),
                "geo" | "probe" => {
                    return Err(format!(
                        "{field} needs an address family: use {field}4 or {field}6"
                    ));
                }
                _ => {
                    return Err(format!(
                        "unknown location field {field:?}; use geo4, geo6, edge, probe4, or probe6"
                    ));
                }
            };
            if repeated {
                return Err(format!("location field {field} appears twice"));
            }
        }
        Ok(constraints)
    }
}

impl Constraints {
    pub fn constrains_geo(&self) -> bool {
        self.geo4.is_some() || self.geo6.is_some()
    }

    fn colos(&self) -> impl Iterator<Item = &String> {
        [&self.edge, &self.probe4, &self.probe6]
            .into_iter()
            .flatten()
    }

    fn places(&self) -> impl Iterator<Item = &Place> {
        [&self.geo4, &self.geo6].into_iter().flatten()
    }

    // Catches typos before any connection, as far as the available data allows.
    pub fn validate(&self, geofeed: Option<&Geofeed>) -> Result<()> {
        let Some(geofeed) = geofeed else {
            return Ok(());
        };
        if let Some(code) = self.colos().find(|code| !geofeed.knows_colo(code)) {
            bail!("Cloudflare has no colo {code}");
        }
        if let Some(place) = self.places().find(|place| !geofeed.knows(place)) {
            bail!("Cloudflare's geofeed has no address in {place}");
        }
        Ok(())
    }

    // Whether a relay that Cloudflare serves from `colo` could satisfy the constraints, as far
    // as that hint tells.
    pub fn plausible(&self, colo: &str, geofeed: Option<&Geofeed>) -> bool {
        let country = geofeed.and_then(|geofeed| geofeed.colo_country(colo));
        self.edge.as_ref().is_none_or(|edge| edge == colo)
            && country.is_none_or(|country| self.places().all(|place| place.country == country))
    }

    pub fn check(&self, locations: &Locations) -> Result<()> {
        for (field, expected, actual) in [
            ("geo4", &self.geo4, &locations.geo4),
            ("geo6", &self.geo6, &locations.geo6),
        ] {
            if let Some(expected) = expected
                && !actual
                    .as_ref()
                    .is_some_and(|actual| expected.contains(actual))
            {
                bail!("{field} is {}, not {expected}", unavailable(actual));
            }
        }
        let probe = |probe: &Option<Probe>| probe.as_ref().map(|probe| probe.colo.clone());
        for (field, expected, actual) in [
            ("edge", &self.edge, Some(locations.edge.clone())),
            ("probe4", &self.probe4, probe(&locations.probe4)),
            ("probe6", &self.probe6, probe(&locations.probe6)),
        ] {
            if let Some(expected) = expected
                && actual.as_ref() != Some(expected)
            {
                bail!("{field} is {}, not {expected}", unavailable(&actual));
            }
        }
        Ok(())
    }
}

fn unavailable<T: fmt::Display>(value: &Option<T>) -> String {
    value
        .as_ref()
        .map_or_else(|| "unavailable".into(), ToString::to_string)
}

// Everything observed about where a connected tunnel sits.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Locations {
    pub geo4: Option<Place>,
    pub geo6: Option<Place>,
    pub edge: String,
    pub probe4: Option<Probe>,
    pub probe6: Option<Probe>,
}

impl Locations {
    pub fn observe(
        edge: String,
        probe4: Option<Probe>,
        probe6: Option<Probe>,
        geofeed: Option<&Geofeed>,
    ) -> Self {
        let geo = |probe: &Option<Probe>| Some(geofeed?.place(probe.as_ref()?.address)?.clone());
        Self {
            geo4: geo(&probe4),
            geo6: geo(&probe6),
            edge,
            probe4,
            probe6,
        }
    }
}

impl fmt::Display for Locations {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "geo4 {}, geo6 {}, edge {}, probe4 {}, probe6 {}",
            unavailable(&self.geo4),
            unavailable(&self.geo6),
            if self.edge.is_empty() {
                "unavailable"
            } else {
                &self.edge
            },
            unavailable(&self.probe4),
            unavailable(&self.probe6)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(text: &str) -> Place {
        text.parse().unwrap()
    }

    #[test]
    fn constraints_name_one_family_each_and_ignore_case() {
        let parsed: Constraints = "GEO4=jp/Tokyo+Edge=nrt".parse().unwrap();
        assert_eq!(parsed.geo4, Some(place("JP/Tokyo")));
        assert_eq!(parsed.edge.as_deref(), Some("NRT"));
        for invalid in [
            "JP",
            "geo=JP",
            "probe=NRT",
            "geo4=JP+geo4=US",
            "geo4=NRT",
            "edge=JP",
        ] {
            assert!(invalid.parse::<Constraints>().is_err(), "{invalid}");
        }
    }

    #[test]
    fn checks_places_by_country_or_normalized_city() {
        let locations = Locations {
            geo4: Some(place("US/Los Angeles")),
            geo6: None,
            edge: "LAX".into(),
            ..Default::default()
        };
        let check = |text: &str| text.parse::<Constraints>().unwrap().check(&locations);
        assert!(check("geo4=us").is_ok());
        assert!(check("geo4=US/losangeles+edge=LAX").is_ok());
        assert!(check("geo4=US/Seattle").is_err());
        assert!(check("geo6=US").is_err());
        assert!(check("probe4=LAX").is_err());
    }
}
