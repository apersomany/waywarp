# Regions and relays

Use `--location` to request a location and `--via` to choose a setup path. For example, to request an IPv4 address advertised in Hong Kong:

```sh
waywarp up proxy --accept-tos --location geo4=HK --via mudfish:city=hongkong
```

Review [Cloudflare's terms](https://www.cloudflare.com/application/terms/) before using `--accept-tos` for a new registration. See [location fields](locations.md) for matching rules and the distinction between IP geolocation, tunnel endpoints, and probe colos.

## Setup paths

| `--via` value | Path |
| --- | --- |
| `direct` | Your own network; the default when no `--via` is given |
| `socks5:ADDRESS:PORT` | A UDP-capable SOCKS5 relay at an IPv4 address |
| `mudfish:FILTER` | Mudfish nodes matching the filter |

Entries are tried in order until a connection satisfies the location requirements. Later providers are queried only when reached. You can mix and repeat entries, except that `direct` may appear only once.

This tries a SOCKS5 relay before searching Mudfish. Replace the example relay address with your own:

```sh
waywarp up proxy --accept-tos --location 'geo4=HK+edge=HKG' \
  --via socks5:192.0.2.1:1080 --via mudfish:hongkong
```

## Moving off the relay

Only setup uses the relay. Waywarp switches WARP's MASQUE/QUIC UDP flows to direct sockets, then verifies the connection and requested locations. WARP may migrate the session or reconnect during the switch; the relay is not a permanent fallback.

In experiments, connections could retain their region after moving onto the local network, consistent with connection-ID-based routing. This is not a documented Cloudflare guarantee. Waywarp does not rewrite connection IDs or assume the region was retained; it checks the result.

After reconnects, Waywarp rechecks locations and repeats setup if a requirement no longer matches, using the same `--via` entries and a fresh Mudfish node list. `--no-rebootstrap` keeps a mismatched connection instead; `status` reports the mismatch and exits `1`.

## Mudfish

[Mudfish](https://mudfish.net) provides UDP-capable SOCKS5 nodes and charges by traffic. Since the relay is used only during setup, its traffic cost can be small.

### Mudfish filters

A plain word searches the whole node location name. `country`, `region`, `city`, `provider`, and `id` select a field.

- `+` or `&` joins positive terms: all must match.
- `-` excludes nodes matching the next term.
- Terms contain letters and digits; matching ignores case, spaces, and punctuation in node names.

| Filter | Selects |
| --- | --- |
| `mudfish:city=hongkong` | Nodes in Hong Kong |
| `mudfish:country=hk-provider=azure` | Hong Kong nodes excluding Azure |
| `mudfish:city=hongkong+provider=azure` | Azure nodes in Hong Kong |

Write `hongkong`, not `hong-kong`: a hyphen starts an exclusion. Quote filters containing `&` in a shell. For alternatives, repeat `--via` rather than joining positive terms.

Waywarp pings nodes four at a time and spreads attempts across providers and cities so one failing provider does not exhaust the search.

### Authentication pacing

All instances in a user's store share a limit of at least six seconds between Mudfish SOCKS5 authentication exchanges, including failed logins, relay pings, colo checks, and tunnel flows. Broad searches can take several minutes. Other programs, other users' stores, and ordinary `socks5:` entries do not share this limit.

## Credentials

Set credentials in the environment, not command-line arguments. Waywarp reads and removes these variables before starting helpers. Set both username and password when authentication is needed:

| Variables | Used for |
| --- | --- |
| `WAYWARP_MUDFISH_USERNAME`, `WAYWARP_MUDFISH_PASSWORD` | `mudfish:` entries |
| `WAYWARP_SOCKS5_USERNAME`, `WAYWARP_SOCKS5_PASSWORD` | `socks5:` entries requiring a login |

For NixOS services, use an [`environmentFile`](nixos.md#credentials) outside the Nix store.
