# Regions and relays

WARP normally connects near you. To exit elsewhere, name the location you need with `--location` and how to reach it with `--via`:

```sh
waywarp up proxy --location geo4=HK --via mudfish:city=hongkong
```

Waywarp connects WARP through a relay in that region, then migrates the QUIC session onto your own network. The session keeps the location it received, and your traffic no longer passes through the relay.

## Locations

`--location` joins `field=VALUE` terms with `+`; every term must match, ignoring case.

| Field | Takes | Checks |
| --- | --- | --- |
| `geo4`, `geo6` | A country, or `COUNTRY/City` | Where Cloudflare's geofeed places the WARP IPv4 or IPv6 address |
| `edge` | A colo | Where the tunnel terminates |
| `probe4`, `probe6` | A colo | Which colo served a test request over IPv4 or IPv6 |

`geo4=HK` is usually what you want: it is what most IP-geolocating websites see. Countries are two-letter codes; colos are Cloudflare's three-letter codes, such as `HKG`. Cities come from Cloudflare's geofeed, so copy them from `status`. [Location fields](locations.md) explains each field and its limits.

## Vias

`--via` lists ways to reach Cloudflare. Waywarp tries them in order until WARP connects with the required locations, so a relay only has to work once:

| Via | Connects |
| --- | --- |
| `direct` | From your own network; the default |
| `socks5:ADDRESS:PORT` | Through a SOCKS5 relay that supports UDP |
| `mudfish:FILTER` | Through matching Mudfish nodes |

Entries can be mixed, and all but `direct` repeated:

```sh
waywarp up proxy --location geo4=HK+edge=HKG --via socks5:192.0.2.1:1080 --via mudfish:hongkong
```

Once connected, the instance keeps its location. Waywarp checks it again whenever WARP reconnects; if it no longer matches, it bootstraps again the same way, with a fresh Mudfish node list. With `--no-rebootstrap`, it keeps the connection and reports the mismatch in `status`, which then exits `1`.

## Mudfish

[Mudfish](https://mudfish.net) has UDP-capable SOCKS5 nodes in many regions and charges by traffic. A bootstrap costs little, because only connection setup goes through the relay: one measured bootstrap sent under 100 KiB, including pings of about thirty nodes. Waywarp is not affiliated with Mudfish.

### Mudfish filters

Waywarp selects nodes by their location names. Plain words match anywhere; `country`, `region`, `city`, `provider`, and `id` match one field. Every positive term must match: `+` (or `&`) joins terms, and `-` excludes nodes matching the following term. Terms are letters and digits, compared against names without case, spaces, or punctuation, so `city=hongkong` matches Hong Kong. Write `hongkong`, not `hong-kong`: `-` would start an exclusion.

Examples:

```sh
mudfish:city=hongkong
mudfish:country=hk-provider=azure
mudfish:city=hongkong+provider=azure
```

Quote filters containing `&` in a shell. For alternatives, repeat `--via` rather than joining them in one filter.

Waywarp pings matching nodes four at a time and spreads attempts across providers and cities, so one bad provider does not stall the search.

### Authentication pacing

Mudfish throttles repeated logins. Waywarp serializes every Mudfish SOCKS5 authentication in a user's store, from relay pings, colo checks, and tunnel flows of every instance, with at least six seconds between exchanges, counting failures. Broad searches can therefore take several minutes. Other programs and other users' stores do not share this limit; `socks5:` vias are not paced.

## Credentials

Relay credentials come from the environment so they stay out of process listings. Waywarp removes them from its environment before starting any helper:

| Variables | For |
| --- | --- |
| `WAYWARP_MUDFISH_USERNAME`, `WAYWARP_MUDFISH_PASSWORD` | `mudfish:` vias |
| `WAYWARP_SOCKS5_USERNAME`, `WAYWARP_SOCKS5_PASSWORD` | `socks5:` vias that need a login |

Under the NixOS module, provide them through `environmentFile`; see [NixOS](nixos.md).
