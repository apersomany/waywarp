// The probe4 and probe6 fields: which colo serves a request over each address family.
use crate::http::Request;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

const URL: &str = "https://engage.cloudflareclient.com/cdn-cgi/trace";
// Pinned addresses keep each probe on one family and avoid proxy DNS ambiguity.
const PROBE4: IpAddr = IpAddr::V4(Ipv4Addr::new(162, 159, 192, 1));
const PROBE6: IpAddr = IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0xd0, 0, 0, 0, 0xa29f, 0xc001));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    V4,
    V6,
}

// What Cloudflare saw for one request: the colo that served it and its source address.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Probe {
    pub colo: String,
    pub address: IpAddr,
}

impl fmt::Display for Probe {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.colo, self.address)
    }
}

// Requests the trace endpoint through a SOCKS5 proxy, or through the calling thread's routes.
pub fn probe(family: Family, proxy: Option<SocketAddr>) -> Result<Probe> {
    let body = Request {
        address: Some(match family {
            Family::V4 => PROBE4,
            Family::V6 => PROBE6,
        }),
        proxy,
        ..Request::new(URL, 16 * 1024)
    }
    .get()
    .context("location probe failed")?;
    let probe = parse(&String::from_utf8(body)?)?;
    if probe.address.is_ipv6() != (family == Family::V6) {
        bail!("the {family:?} location probe returned {}", probe.address);
    }
    Ok(probe)
}

fn parse(text: &str) -> Result<Probe> {
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
            .map(str::trim)
    };
    Ok(Probe {
        colo: field("colo")
            .context("the location probe had no colo")?
            .to_ascii_uppercase(),
        address: field("ip")
            .context("the location probe had no IP address")?
            .parse()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_fields_are_parsed() {
        let probe =
            parse("fl=1\nh=engage.cloudflareclient.com\nip=104.28.211.30\ncolo=nrt\nwarp=on\n")
                .unwrap();
        assert_eq!(probe.to_string(), "NRT/104.28.211.30");
        assert!(parse("ip=104.28.211.30\n").is_err());
    }
}
