# Waywarp

Waywarp is a tool for orchestrating multiple Cloudflare WARP connections on one Linux machine.

# Features

- Run multiple independent WARP connections on one host.
- Support for both tunnel mode via a bridge and proxy mode.
- Import existing registrations.
- Optionally bootstrap regional connections through a setup proxy, then connect directly to Cloudflare.

# Install

```sh
curl -fsSL https://raw.githubusercontent.com/apersomany/waywarp/master/install.sh | bash
```

Should work on most Linux distributions with [Cloudflare WARP](https://developers.cloudflare.com/warp-client/get-started/linux/), iproute2, and nftables installed.

## Nix

```sh
nix profile add github:apersomany/waywarp
```

## From source

```sh
cargo install --locked --git https://github.com/apersomany/waywarp
```

# Usage

Read [Cloudflare’s Terms of Service](https://www.cloudflare.com/application/terms/) before accepting them with `--accept-tos`.

## Proxy mode

```sh
waywarp up proxy 0 --name personal --accept-tos
waywarp up proxy 1 --name work --accept-tos
```

Configure applications to use `127.0.0.1:1080` or `127.0.0.1:1081` as their SOCKS5 or HTTP proxy.

```sh
curl -x socks5h://127.0.0.1:1080 https://www.cloudflare.com/cdn-cgi/trace
```

Check or stop connections:

```sh
waywarp status
waywarp down personal
waywarp down work
```

## Tunnel mode

```sh
sudo waywarp up bridge --accept-tos
sudo curl --interface waywarp0 https://www.cloudflare.com/cdn-cgi/trace
sudo waywarp down
```

Bind applications to `waywarp0` or configure routes through it.

## Importing a registration

Import the host client’s registration, disconnect the original while the copy makes its first connection, then reconnect it:

```sh
sudo waywarp import 3
warp-cli --accept-tos disconnect
sudo waywarp up bridge 3 --name work
warp-cli --accept-tos connect
```

For Zero Trust registrations, your organization’s policies still apply.

## Regional bootstrapping

Use a UDP-capable SOCKS5 proxy in the target region:

```sh
waywarp up proxy 2 --name hong-kong --accept-tos \
  --location geo4=HK --via socks5:ADDRESS:PORT
```

Replace `ADDRESS:PORT` with your proxy’s IPv4 address and port. The proxy is only used for setup, then traffic goes directly to Cloudflare.

### Mudfish

There’s also native [Mudfish](https://mudfish.net) integration because its traffic-based pricing makes setup-only use ridiculously cheap.

```sh
export WAYWARP_MUDFISH_USERNAME='your-username'
export WAYWARP_MUDFISH_PASSWORD='your-password'

waywarp up proxy 2 --name hong-kong --accept-tos \
  --location geo4=HK --via mudfish:city=hongkong
```

The local proxy listens on `127.0.0.1:1082` in either case.

# Understanding locations

IP routing does not require symmetric paths, and asymmetry is normal on anycast networks like Cloudflare. Traffic can enter and leave through different data centers, so the tunnel endpoint does not necessarily identify the exit.

| Field | What it measures | What it implies |
| --- | --- | --- |
| `edge` | The Cloudflare-side tunnel endpoint reported by `warp-cli tunnel stats`. | The tunnel’s ingress region, not necessarily where traffic exits Cloudflare. |
| `geo4`, `geo6` | The public IPv4/IPv6 addresses returned by the probes, looked up in [Cloudflare’s IP geofeed](https://api.cloudflare.com/local-ip-ranges.csv). | The IPs’ advertised country and city, not necessarily their physical exit. |
| `probe4`, `probe6` | The data center (`colo`) and public source IP returned by an IPv4/IPv6 Cloudflare trace request through WARP. | Where Cloudflare served that test request, which is a clue to likely egress rather than proof that every destination uses the same exit. |

**All five fields can differ.** IPv4 and IPv6 can use different paths, and websites using other geolocation databases may place the same IP elsewhere.

For example, `geo4=HK` with `probe4=LAX` means the IPv4 address is advertised in Hong Kong, but the test request was served in Los Angeles.

## Requiring a location

`--location` accepts one field or several joined with `+`. Every term must match.

```sh
--location 'geo4=HK+geo6=HK+edge=HKG'
```

This requires both public IP families to be advertised in Hong Kong and the tunnel endpoint to be HKG. The earlier `geo4=HK` example only requires the IPv4 location.

- `geo4` and `geo6` accept two-letter country codes or `COUNTRY/City`.
- `edge`, `probe4`, and `probe6` accept three-letter colo codes such as `HKG`.

An unavailable required field fails the check, so only require the address families you need.

Waywarp checks after moving off the setup proxy and after reconnects. If a required location changes, it repeats setup.

See [location fields](docs/locations.md) for matching rules and further details.

# Documentation

- [Instances, logs, and troubleshooting](docs/instances.md)
- [Bridge routing, NAT, and Zero Trust](docs/access.md)
- [Regional bootstrapping, filters, and credentials](docs/regions.md)
- [Location fields](docs/locations.md)
- [NixOS services](docs/nixos.md)
- [Terminal output and stream contracts](docs/terminal-output.md)
- [WARP routing observations](docs/routing.md)
- [Architecture](docs/architecture.md)
- [Changelog](CHANGELOG.md)

# Why I built it

I use WARP for both work and my personal network, but the official client makes me switch between them. I tried running separate clients in systemd-managed namespaces, but that setup felt cumbersome and unreliable. I wanted a small Rust wrapper with Nix integration, so I built Waywarp.

# Efficiency

Keeping the official client means running a daemon for each connection and adding a userspace hop for tunnel packets. A kernel WireGuard setup can be leaner where available, but it does not offer the same MASQUE and Zero Trust support.

I’ve tried to keep the overhead down, but there’s a limit to what a wrapper can do without becoming a much larger project.

# Development and license

```sh
nix develop --command cargo test --locked
nix flake check
```

The flake checks formatting, Clippy, package tests, the installer, and a NixOS VM with a WARP stand-in. These checks do not test live WARP or setup-proxy availability.

[MIT](LICENSE). Independent project, not affiliated with Cloudflare or Mudfish.
