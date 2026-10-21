# Location fields

The tunnel endpoint, a public IP's advertised location, and the colo serving a request can differ. IPv4 and IPv6 can differ too. Use the field that measures what you need; see [regions and relays](regions.md) for setup examples.

## Fields

| Field | Source and meaning |
| --- | --- |
| `edge` | Colo from `warp-cli tunnel stats`: the Cloudflare-side tunnel endpoint, not necessarily the traffic exit |
| `geo4`, `geo6` | Public IP from each probe, looked up in [Cloudflare's IP geofeed](https://api.cloudflare.com/local-ip-ranges.csv): its advertised country and optional city |
| `probe4`, `probe6` | Public IP and `colo` from each trace request: where Cloudflare served that test request, a clue to likely egress rather than proof for every destination |

Probes request `https://engage.cloudflareclient.com/cdn-cgi/trace` through WARP, using pinned addresses for each family. Proxy instances use WARP's proxy; bridge instances use the namespace's WARP routes.

Geofeed places are not colos, and websites using other geolocation databases may place the same IP elsewhere. If a website reports an unexpected country, check which address family it uses and compare the corresponding `geo` field. For traffic-path investigation, compare `edge` with both probe colos without assuming other destinations follow the same path.

A missing probe leaves that family's probe and geolocation fields unavailable. Missing geofeed data can leave a `geo` field unavailable even when its probe succeeds.

## Constraints

`--location` accepts `field=VALUE` terms joined with `+`. Every term must match.

| Requirement | Meaning |
| --- | --- |
| `geo4=HK` | The public IPv4 address is advertised in Hong Kong |
| `geo4=HK+geo6=HK+edge=HKG` | Both IP families are advertised in Hong Kong and the tunnel endpoint is HKG |
| `geo4=HK+probe4=HKG+probe6=HKG` | The IPv4 address is advertised in Hong Kong and both probes are served by HKG |

Matching rules:

- Field names and values ignore case.
- `geo4` and `geo6` accept a two-letter country code or `COUNTRY/City`. A country matches any city in it. City matching also ignores spaces and punctuation: `US/losangeles` matches Los Angeles.
- `edge`, `probe4`, and `probe6` accept three-letter colo codes, such as `HKG`.
- Each field may appear only once. Use family-specific names; `geo` and `probe` are not accepted.
- An unavailable required field fails the check. Unconstrained fields are still reported but do not affect matching, so only require the address families you need.

When location data is available, Waywarp rejects unknown countries, cities, and colos before connecting. It verifies actual locations after setup and migration, then after reconnects. See [moving off the relay](regions.md#moving-off-the-relay) for rebootstrap behavior.

## Data cache

Cloudflare's geofeed and colo catalog are cached for 24 hours and shared by instances in the same store. If a refresh fails, existing cached data is used when possible.

Without a `geo4` or `geo6` requirement, unavailable location data leaves those fields unavailable rather than preventing startup. A geolocation requirement needs that data, so startup fails if it cannot be loaded.
