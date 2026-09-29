use anyhow::{Context, Result};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use ureq::Agent;
use ureq::http::Uri;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

const TIMEOUT: Duration = Duration::from_secs(15);

pub struct Request<'a> {
    pub url: &'a str,
    pub limit: u64,
    pub headers: &'a [(&'a str, &'a str)],
    // Connects here instead of resolving the host; TLS still verifies the URL's host name.
    pub address: Option<IpAddr>,
    pub proxy: Option<SocketAddr>,
}

impl<'a> Request<'a> {
    pub fn new(url: &'a str, limit: u64) -> Self {
        Self {
            url,
            limit,
            headers: &[],
            address: None,
            proxy: None,
        }
    }

    pub fn get(&self) -> Result<Vec<u8>> {
        let proxy = self
            .proxy
            .map(|proxy| ureq::Proxy::new(&format!("socks5://{proxy}")))
            .transpose()?;
        let config = Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .proxy(proxy)
            .build();
        let resolver = Pinned(self.address.map(|address| (host(self.url), address)));
        let agent = Agent::with_parts(config, DefaultConnector::new(), resolver);
        let mut request = agent.get(self.url);
        for (name, value) in self.headers {
            request = request.header(*name, *value);
        }
        let mut response = request
            .call()
            .with_context(|| format!("fetching {}", self.url))?;
        Ok(response
            .body_mut()
            .with_config()
            .limit(self.limit)
            .read_to_vec()?)
    }
}

fn host(url: &str) -> String {
    url.parse::<Uri>()
        .ok()
        .and_then(|uri| uri.host().map(str::to_owned))
        .unwrap_or_default()
}

// Answers lookups of one host with a fixed address, which also fixes the address family. Other
// hosts, such as the proxy itself, resolve normally. Plain SOCKS5 resolves before connecting, so a
// proxied request goes to the same address.
#[derive(Debug)]
struct Pinned(Option<(String, IpAddr)>);

impl Resolver for Pinned {
    fn resolve(
        &self,
        uri: &Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let Some((_, address)) = self.0.as_ref().filter(|(host, _)| uri.host() == Some(host))
        else {
            return DefaultResolver::default().resolve(uri, config, timeout);
        };
        let port = uri.port_u16().unwrap_or(match uri.scheme_str() {
            Some("http") => 80,
            _ => 443,
        });
        let mut addresses = self.empty();
        addresses.push(SocketAddr::new(*address, port));
        Ok(addresses)
    }
}
