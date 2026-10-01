# Location fields

WARP does not have a single location. The tunnel endpoint, the advertised location of a public IP, and the path taken by a request can all differ. IPv4 and IPv6 need not agree either.

Waywarp reports five fields to keep these distinctions visible. This guide explains how to read and request them. See [regions and relays](regions.md) for connection examples, or [routing observations](routing.md) for the experiments and proposed explanation.

## Fields

### `edge`

**Meaning:** the Cloudflare-side endpoint of the WARP tunnel, or its tunnel ingress region. Use it when you care where the tunnel session terminates, rather than where a website places your public IP.

**Source:** the edge colo reported by `warp-cli tunnel stats`.

This is a colo code, such as `HKG`. It does not describe the whole path through Cloudflare or establish where traffic to every destination leaves the network.

### `geo4` and `geo6`

**Meaning:** the country and, when available, city associated with the public source IP for each address family. These describe the connection's IP geolocation identity, not necessarily the physical exit.

**Source:** the public IP returned by each trace probe, looked up in [Cloudflare's IP geofeed](https://api.cloudflare.com/local-ip-ranges.csv).

Use these fields when you want an IPv4 or IPv6 address advertised as belonging to a particular country or city. Websites using other geolocation databases may place the same address elsewhere.

Geofeed places are not colos. Cloudflare advertises more places than it has colos, so an IP's city need not match the city of the tunnel endpoint. Each address family has its own public IP; `geo4` and `geo6` can differ.

### `probe4` and `probe6`

**Meaning:** likely egress colos for the IPv4 and IPv6 test requests. These are clues to the traffic path, rather than the IP's advertised location.

**Source:** the `colo` and `ip` fields returned by IPv4 and IPv6 requests to `https://engage.cloudflareclient.com/cdn-cgi/trace` through WARP. The probes use pinned addresses to keep each request on its intended address family. Proxy instances probe through WARP's proxy; bridge instances use the namespace's WARP routes.

The reported value includes both the colo and the public source IP. The colo identifies where Cloudflare served that request. Treating it as an egress location is an interpretation, not proof that every destination uses that exit. Cloudflare can route different address families and destinations differently.

## Reading the results

**All five location fields can be different.** That does not, by itself, mean the connection is broken.

- If a website places you in the wrong country, start with `geo4` and `geo6`, then check which address family and geolocation database the website uses.
- If you want a particular tunnel endpoint, check `edge`.
- If you are investigating the actual traffic path, compare `probe4` and `probe6` with `edge`. Do not assume the probe path applies to other destinations.

A missing probe leaves that family's probe and geolocation fields unavailable. Missing geofeed data can leave `geo4` and `geo6` unavailable even when the probes succeed.

## Constraints

`--location` accepts `field=VALUE` terms joined with `+`. Every term must match. Field names and matching values ignore case.

Examples of different requirements:

| Requirement | Meaning |
| --- | --- |
| `geo4=HK` | The public IPv4 address is advertised as being in Hong Kong |
| `geo4=HK+geo6=HK+edge=HKG` | Both public IP families are advertised as being in Hong Kong, and the tunnel endpoint is HKG |
| `geo4=HK+probe4=HKG+probe6=HKG` | The IPv4 address is advertised as being in Hong Kong, and both probes are served by HKG |

To use the second requirement:

```sh
waywarp up proxy --location 'geo4=HK+geo6=HK+edge=HKG' --via mudfish:city=hongkong
```

`geo4` and `geo6` take a two-letter country code or `COUNTRY/City`. A country matches any city in it. City matching ignores case, spaces, and punctuation, so `US/losangeles` matches Los Angeles. `edge`, `probe4`, and `probe6` take three-letter colo codes.

Each field can appear only once. Use the full family-specific names: `geo` and `probe` are not accepted.

Every field except `edge` refers to one address family. An unavailable field fails a requirement on it, so only require the families you need. Fields you do not constrain are still reported, but do not affect whether the location check passes.

When Cloudflare's location data is available, Waywarp validates country/city values and colo codes before connecting. Unknown values fail early. It checks the actual connection after setup and migration, then again after later reconnects.

If a required field changes, Waywarp repeats setup using the ordered `--via` entries. With `--no-rebootstrap`, it keeps the connection and reports the mismatch instead.

## Data cache

Cloudflare's geofeed and colo catalog are cached for 24 hours and shared by instances in the same store. If a refresh fails, Waywarp uses existing cached data when it can.

Without a `geo4` or `geo6` requirement, a failure to load the location data leaves those fields unavailable rather than preventing startup. A geolocation requirement needs that data to be checked, so startup fails if it cannot be loaded.
