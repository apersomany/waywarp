# Waywarp

Waywarp is a Linux tool for running separate Cloudflare WARP clients. Each has its own registration, daemon, and network namespace, so work and personal connections can run side by side. Use a local SOCKS5/HTTP proxy or a dedicated network link without changing your machine's default connection or interfering with an existing WARP client.

Waywarp can also establish a connection through a relay in another region, then move it onto your own network. Traffic then goes directly to Cloudflare, without passing through the relay. In practice, this lets us choose an exit region using a cheap proxy only for connection setup.

Getting the region you want depends on WARP and the available relays. Waywarp checks the resulting location rather than assuming it worked.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/apersomany/waywarp/master/install.sh | sh
```

[Inspect the installer](install.sh) before running it. It downloads the matching [release binary](https://github.com/apersomany/waywarp/releases), verifies its checksum, and installs it to `/usr/local/bin`, asking for `sudo` if needed. Set `WAYWARP_PREFIX` to change the prefix or `WAYWARP_VERSION` to pin a release tag.

Requires **x86_64 or aarch64 Linux**, `/dev/net/tun`, and these tools on `PATH`:

| Tools | Package |
| --- | --- |
| `warp-svc`, `warp-cli` | [Cloudflare WARP](https://developers.cloudflare.com/warp-client/get-started/linux/) |
| `ip` | iproute2 |
| `nft` | nftables |

Waywarp starts its own daemons, so the host's WARP service does not need to run. To use a proxy without root, you need unprivileged user namespaces, `XDG_RUNTIME_DIR`, and WARP's state directories to exist. Otherwise, use `sudo`. Bridge access always needs root. See [setup and troubleshooting](docs/instances.md#setup-and-troubleshooting).

With **Nix**, the package includes WARP and the runtime tools:

```sh
nix profile install github:apersomany/waywarp
```

To build **from source**, use Rust 1.89 or newer. The runtime tools are still required:

```sh
cargo install --locked --git https://github.com/apersomany/waywarp
```

## Supported features

- **Separate clients:** each instance keeps its own registration and state, with an index from `0` to `255` and an optional name.
- **Local proxies:** SOCKS5 and HTTP CONNECT on loopback, for normal users or root. Each instance has its own listen address.
- **Network links:** IPv4/IPv6 links for applications bound to an interface or traffic routed through it. Supports configurable NAT and incoming traffic for the device's assigned WARP addresses. Requires root.
- **Regional exits:** use UDP-capable SOCKS5 relays or filtered Mudfish nodes for setup only. Try multiple relays in order, and repeat setup if a required location changes after reconnecting.
- **Zero Trust:** import an existing Team registration without enrolling again, including support for policy and posture requests. Access still follows your organization's policy.
- **Status and control:** check locations and connection health, get JSON output, or run `warp-cli` against an individual instance.
- **Services:** run in the foreground with systemd readiness notification, or manage named instances through the NixOS module.

Waywarp uses WARP's MASQUE protocol. Tunnel traffic goes through the host's physical network, bypassing host VPNs. New flows follow network changes; use `--interface` to choose a particular link.

## Usage

Creating a new registration requires accepting [Cloudflare's Terms of Service](https://www.cloudflare.com/application/terms/). After reviewing them, pass `--accept-tos` as shown below. Saved and imported registrations do not require the flag again.

### Local proxies (rootless)

Start two independent clients as your normal user, without `sudo`:

```sh
waywarp up proxy 0 --name personal --accept-tos
waywarp up proxy 1 --name second --accept-tos

curl -x socks5h://127.0.0.1:1080 https://www.cloudflare.com/cdn-cgi/trace
curl -x socks5h://127.0.0.1:1081 https://www.cloudflare.com/cdn-cgi/trace
```

The index is the instance's primary key; a name is an optional alias. Renaming an instance with `--name` does not change its index, registration, default proxy port, or bridge link. Names must be unique within a user's store, and the old name stops working after a rename.

The default proxy port is `1080 + INDEX`. Both SOCKS5 and HTTP CONNECT use the same listener; `--listen` changes its loopback address and port.

`up` waits for a healthy connection and prints the observed locations. The first run registers a device; later runs reuse the registration. Look for `warp=on` in the trace.

```sh
waywarp status
waywarp status personal --json
waywarp warp-cli personal tunnel stats
waywarp down personal
waywarp down second
```

Stopping keeps the registration and name, but startup options are not saved. Repeat access, location, and relay options when starting again. Root and each user have separate stores: if you start an instance with `sudo`, use it for later commands too.

**The proxy has no authentication.** Any local user can use it, including an imported Zero Trust identity. On shared machines, prefer bridge access for those identities.

### Another region

With a [Mudfish](https://mudfish.net) account, set `WAYWARP_MUDFISH_USERNAME` and `WAYWARP_MUDFISH_PASSWORD` in your environment, then:

```sh
waywarp up proxy 2 --name hong-kong --accept-tos --location geo4=HK --via mudfish:city=hongkong
curl -x socks5h://127.0.0.1:1082 https://www.cloudflare.com/cdn-cgi/trace
waywarp status hong-kong
waywarp down hong-kong
```

This requires an IPv4 source IP advertised as being in Hong Kong. It does not require IPv6 or the tunnel endpoint to be there; see [choosing and interpreting locations](#choosing-and-interpreting-locations).

A UDP-capable SOCKS5 relay also works with `--via socks5:ADDRESS:PORT`. Broad Mudfish searches can take minutes because logins are paced. See [regions and relays](docs/regions.md) for credentials, filters, alternatives, and reconnection behavior.

### A network link

```sh
sudo waywarp up bridge --accept-tos
sudo curl --interface waywarp0 https://www.cloudflare.com/cdn-cgi/trace
sudo waywarp down
```

Bridge access creates `waywarp0` in this example, or `waywarpINDEX` for another index. Applications bound to it use WARP; other traffic uses it only when you add routes. Waywarp adds no host firewall rules. See [bridge access and Zero Trust](docs/access.md) for routing, NAT, and importing a work registration.

## Choosing and interpreting locations

WARP does not have just one location. Traffic enters Cloudflare through a tunnel, uses a public source IP, and may leave through another Cloudflare colo (data center). IPv4 and IPv6 can take different paths.

Waywarp reports five fields:

| Field | Meaning | Source |
| --- | --- | --- |
| `edge` | The tunnel's Cloudflare-side endpoint, or tunnel ingress region | The edge colo in `warp-cli tunnel stats` |
| `geo4`, `geo6` | The country and city associated with the public IPv4/IPv6 source IP seen by websites | The source IP returned by a trace request, looked up in [Cloudflare's IP geofeed](https://api.cloudflare.com/local-ip-ranges.csv) |
| `probe4`, `probe6` | Likely traffic egress colos for the IPv4/IPv6 test requests, rather than the advertised IP locations | The `colo` field from requests to Cloudflare's `/cdn-cgi/trace` endpoint through WARP, one per address family |

**All five can differ.** Use `geo4`/`geo6` for IP geolocation, `edge` for the tunnel endpoint, and `probe4`/`probe6` as clues to the actual traffic path.

The geofeed describes an IP's advertised location, not necessarily its physical exit. Websites using other geolocation databases may disagree. A probe shows which colo served that request; it does not prove that all destinations leave Cloudflare there.

`--location` takes one field, such as `geo4=HK`, or several joined with `+`. Every term must match. To require Hong Kong for both public IP families and HKG for the tunnel endpoint:

```sh
waywarp up proxy --accept-tos --location 'geo4=HK+geo6=HK+edge=HKG' --via mudfish:city=hongkong
```

`geo4` and `geo6` accept a two-letter country code or `COUNTRY/City`. The other fields accept a three-letter colo code such as `HKG`. A required field that is unavailable fails the check, so only require the address families you need.

Waywarp checks the locations after moving off the relay and after later reconnects. If a required location changes, it repeats connection setup. With `--no-rebootstrap`, it keeps the connection and reports the mismatch instead.

`status` exits `1` unless every reported instance has a currently verified connection and meets its location requirements. A connected tunnel awaiting fresh verification is reported as degraded. A healthy status does not by itself mean every destination is reachable through a bridge; routing and NAT still matter.

See [location fields](docs/locations.md) for matching rules, data sources, and limitations.

## Documentation

- [Instances, logs, and troubleshooting](docs/instances.md)
- [Terminal output and stream contracts](docs/terminal-output.md)
- [Regions, relays, filters, and credentials](docs/regions.md)
- [Location fields](docs/locations.md)
- [WARP routing observations](docs/routing.md)
- [Bridge routing, NAT, and Zero Trust](docs/access.md)
- [NixOS services](docs/nixos.md)
- [Architecture](docs/architecture.md)
- [Changelog](CHANGELOG.md)

## Why I built it

I use WARP for both work and my personal network, but the official client makes me switch between them. I tried running separate clients in systemd-managed namespaces, but that setup felt cumbersome and unreliable. I wanted a small Rust wrapper around Linux namespaces, nftables, and iproute2, with Nix integration, so I built Waywarp.

## Efficiency

Keeping the official client has a cost: a daemon for every instance and an extra userspace hop for tunnel packets. Using WARP credentials with the kernel WireGuard driver should be leaner, where that's an option, but it doesn't offer the same MASQUE and Zero Trust support.

I've tried to avoid unnecessary overhead with things like passing open sockets between processes and reusing packet buffers. There's still a limit to what a wrapper can do, and I don't want to turn this into a much larger project just to squeeze out a little more performance.

I'd like to try using eBPF/kprobes to intercept WARP's network configuration. If that works out, it could replace bridge mode with something more efficient. For now, it's just an idea, not something to count on.

## Development and license

```sh
nix develop --command cargo test --locked
nix flake check
```

The flake checks formatting, Clippy, package tests, the installer against a fixture release, and a NixOS VM using a WARP stand-in. These checks do not test live WARP or relay availability.

[MIT](LICENSE). Independent project; not affiliated with Cloudflare or Mudfish.
