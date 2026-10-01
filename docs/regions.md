# Regions and relays

WARP normally connects near you. To try another region, use `--location` for what you want and `--via` for a way to connect from there:

```sh
waywarp up proxy --accept-tos --location geo4=HK --via mudfish:city=hongkong
```

`--accept-tos` accepts [Cloudflare's terms](https://www.cloudflare.com/application/terms/) when creating a new registration. Review them before running the example; an existing registration does not need the flag.

Waywarp connects through a relay, then moves the connection onto your own network. The relay is only needed during setup. The location can carry over, but Waywarp checks the result rather than treating the relay's location as a guarantee.

## Locations

`--location` accepts `field=VALUE` terms joined with `+`. Every term must match, and matching ignores case.

| Field | Accepts | Meaning |
| --- | --- | --- |
| `geo4`, `geo6` | A country code, or `COUNTRY/City` | The advertised location of the public IPv4 or IPv6 source IP |
| `edge` | A colo code | The tunnel's Cloudflare-side endpoint |
| `probe4`, `probe6` | A colo code | Likely egress colos for the IPv4 or IPv6 test requests |

For IP geolocation, start with a requirement such as `geo4=HK`. This checks Cloudflare's advertised location for the public IPv4 address, not the tunnel endpoint or physical exit. Websites using other geolocation databases may disagree.

Countries use two-letter codes. Cloudflare colos (data centers) use three-letter codes, such as `HKG`. City names come from Cloudflare's geofeed; you can copy them from `status`.

See [location fields](locations.md) for what each field means, where its value comes from, and which checks to use. IPv4 and IPv6 can differ, and a probe does not establish the exit for every destination.

## Vias

`--via` tells Waywarp how to reach Cloudflare during setup:

| Value | Connection path |
| --- | --- |
| `direct` | Your own network; the default when no `--via` is given |
| `socks5:ADDRESS:PORT` | A SOCKS5 relay with UDP support, at an IPv4 address |
| `mudfish:FILTER` | Mudfish nodes matching the filter |

Waywarp tries entries in order until a connection satisfies the requested locations. You can mix types and repeat entries, except that `direct` can appear only once.

For example, this tries a SOCKS5 relay before searching Mudfish nodes. Replace the example relay address with your own:

```sh
waywarp up proxy --accept-tos --location geo4=HK+edge=HKG --via socks5:192.0.2.1:1080 --via mudfish:hongkong
```

A relay is not kept as a permanent fallback path. After a successful setup, traffic goes directly to Cloudflare. Waywarp checks the locations again whenever WARP reconnects. If a required field changes, it repeats setup with the same `--via` entries and a fresh Mudfish node list.

With `--no-rebootstrap`, Waywarp keeps the connection even if a required location changes. `status` reports the mismatch and exits `1`.

## Mudfish

[Mudfish](https://mudfish.net) offers UDP-capable SOCKS5 nodes in many regions and charges by traffic. Since only setup uses the relay, the traffic cost can be small. One measured setup sent under 100 KiB, including pings of about thirty nodes; that is an example, not a fixed cost. Waywarp is not affiliated with Mudfish.

### Mudfish filters

Filters match node location names. A plain word searches the whole name. `country`, `region`, `city`, `provider`, and `id` select a particular field.

- `+` joins requirements: every positive term must match. `&` means the same thing.
- `-` excludes nodes matching the next term.
- Terms contain letters and digits. Matching ignores case, spaces, and punctuation in node names.

For example:

| Filter | Selects |
| --- | --- |
| `mudfish:city=hongkong` | Nodes in Hong Kong |
| `mudfish:country=hk-provider=azure` | Nodes in Hong Kong, excluding Azure |
| `mudfish:city=hongkong+provider=azure` | Azure nodes in Hong Kong |

Write `hongkong`, not `hong-kong`: the hyphen would start an exclusion. Quote filters containing `&` in a shell. To try alternatives, repeat `--via`; joining positive terms in one filter means all of them must match.

Waywarp pings matching nodes four at a time and spreads connection attempts across providers and cities. A failed provider should not use up the whole search before another provider gets a turn.

### Authentication pacing

Mudfish limits repeated logins. Waywarp allows at least six seconds between SOCKS5 authentication exchanges, including failed ones. All instances in a user's store share this limit, whether a login is for a relay ping, a colo check, or a tunnel flow.

Broad searches can therefore take several minutes. Other programs and other users' stores do not share the limiter. Ordinary `socks5:` entries are not paced.

## Credentials

Set relay credentials in the environment, not in command-line arguments. Waywarp reads and removes these variables before starting helper processes:

| Variables | Used for |
| --- | --- |
| `WAYWARP_MUDFISH_USERNAME`, `WAYWARP_MUDFISH_PASSWORD` | `mudfish:` entries |
| `WAYWARP_SOCKS5_USERNAME`, `WAYWARP_SOCKS5_PASSWORD` | `socks5:` entries that require a login |

For NixOS services, use `environmentFile`; see [NixOS](nixos.md).

For the observations behind relay setup and connection migration, see [WARP routing observations](routing.md).
