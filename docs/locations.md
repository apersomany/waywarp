# Location fields

Waywarp reports several locations because Cloudflare can use different places for the tunnel, the public IP identity, and individual network paths. The [README](../README.md#regions) covers basic use.

## Fields

### `geo4` and `geo6`

The advertised places of the WARP IPv4 and IPv6 addresses: a country and, usually, a city. This is what most websites using IP geolocation see.

Waywarp looks the addresses up in Cloudflare's [IP geofeed](https://api.cloudflare.com/local-ip-ranges.csv). Places are not colos. The geofeed names thousands of places, far more than Cloudflare has colos, and an address served from NRT may be advertised as Tokyo, Narita, or Okubo-naka. Each family has its own address, so `geo4` and `geo6` can differ.

### `edge`

The colo where the WARP tunnel terminates. Waywarp reads it from `warp-cli tunnel stats`.

### `probe4` and `probe6`

The colos returned by IPv4 and IPv6 requests to Cloudflare's `/cdn-cgi/trace` endpoint, with the address each request came from. They are likely exit locations for those probe requests.

A probe does not prove that all destinations use the same exit. Cloudflare can route different address families and destinations differently.

## Constraints

`--location` joins `field=VALUE` terms with `+`, and every term must match. Field names and values ignore case:

```sh
waywarp up proxy --location geo4=JP
waywarp up proxy --location geo4=JP+geo6=JP+edge=NRT
waywarp up proxy --location 'geo4=JP/Tokyo+probe4=KIX+probe6=NRT'
```

`geo4` and `geo6` take a country code, which matches every city in it, or `COUNTRY/City`. Cities compare like Mudfish filters, ignoring case, spaces, and punctuation, so `US/losangeles` matches Los Angeles. `edge`, `probe4`, and `probe6` take a colo code.

Every field except `edge` names one address family. A family that is unavailable fails any constraint on it, so only require the families you need.

Waywarp checks colos and places against Cloudflare's data before connecting, so a typo fails immediately. It checks the constraints after connection and migration, and after later reconnects. A mismatch triggers the ordered bootstrap again unless `--no-rebootstrap` is set. Unconstrained fields are informational.

## Data cache

Waywarp caches Cloudflare's geofeed and colo catalog for 24 hours, shared by every instance in a store. If refresh fails, it uses existing cached data when available. Without a `geo4` or `geo6` constraint, missing data only leaves those fields unavailable.
