# Waywarp

**Independent Cloudflare WARP clients. Regional exits without a permanent relay.**

Waywarp gives each WARP client its own registration, daemon, and network namespace. Use one as a local SOCKS5/HTTP proxy or a routable network link, without turning WARP into your host's default VPN or disturbing an existing client.

Want a different region? Bootstrap WARP through a relay there, then move the tunnel onto your own network. The WARP session keeps its location; subsequent traffic goes directly to Cloudflare, not through the relay. Availability depends on WARP and the relay: Waywarp checks your requested location rather than assuming it worked.

- **Per-application VPN:** point an application at a local proxy.
- **Several exits at once:** give independent instances names such as `home` and `hong-kong`.
- **Selective routing and Zero Trust:** use a root-owned bridge, including an imported Team registration.
- **Declarative services:** run named instances with the [NixOS module](docs/nixos.md).

## Install

Requires **x86_64 or aarch64 Linux**, `/dev/net/tun`, and these tools on `PATH`:

| Tools | Source |
| --- | --- |
| `warp-svc`, `warp-cli` | [Cloudflare WARP](https://developers.cloudflare.com/warp-client/get-started/linux/) |
| `ip` | iproute2 |
| `nft` | nftables |

The WARP system service need not run: Waywarp starts its own daemons. Normal-user proxy access requires unprivileged user namespaces and WARP's state directories to exist; otherwise use `sudo`. Bridge access always requires root. See [setup and troubleshooting](docs/instances.md#setup-and-troubleshooting).

**Nix** includes WARP and the runtime tools:

```sh
nix profile install github:apersomany/waywarp
```

**From source**, with Rust 1.89 or newer and the runtime tools installed:

```sh
cargo install --locked --git https://github.com/apersomany/waywarp
```

For [published releases](https://github.com/apersomany/waywarp/releases), `install.sh` downloads the matching binary and verifies its checksum. Inspect the script before running it; `WAYWARP_PREFIX` chooses the install directory and `WAYWARP_VERSION` pins a tag. Runtime tools are still required.

## Try a local proxy

```sh
waywarp up proxy
curl -x socks5h://127.0.0.1:1080 https://www.cloudflare.com/cdn-cgi/trace
waywarp down
```

`up` waits for a healthy WARP connection and prints its observed locations. In the trace, look for `warp=on`; the first run takes longer because it registers a device. Later runs reuse that registration. If you start with `sudo`, use it for `status` and `down` too.

The proxy accepts SOCKS5 and HTTP CONNECT on `127.0.0.1:1080` by default. It has **no authentication**: any local user can use it, including an imported Zero Trust identity. On shared hosts, prefer bridge access for those identities.

## Try another region

With a [Mudfish](https://mudfish.net) account, put `WAYWARP_MUDFISH_USERNAME` and `WAYWARP_MUDFISH_PASSWORD` in your environment, then:

```sh
waywarp up proxy 1 --name hong-kong --location geo4=HK --via mudfish:city=hongkong
curl -x socks5h://127.0.0.1:1081 https://www.cloudflare.com/cdn-cgi/trace
waywarp status hong-kong
waywarp down hong-kong
```

`geo4=HK` requires the WARP IPv4 exit to geolocate to Hong Kong. Mudfish is used only for connection setup, not ongoing traffic; broad searches can take minutes because logins are paced. A UDP-capable SOCKS5 relay also works with `--via socks5:ADDRESS:PORT`.

See [regions and relay credentials](docs/regions.md) for filters, IPv6 constraints, fallback relays, and reconnection behavior. Waywarp sends new tunnel flows through the host's physical network, bypassing host VPNs; use `--interface` to pin a link.

## Use a network link

```sh
sudo waywarp up bridge
sudo curl --interface waywarp0 https://www.cloudflare.com/cdn-cgi/trace
sudo waywarp down
```

Bridge access creates `waywarp0`. Applications bound to it use WARP; other traffic uses it only when you add routes. Waywarp installs no host firewall rules. See [bridge routing and Zero Trust](docs/access.md) for gateways, NAT policy, connector services, and importing registrations.

## Reference and development

- [Instances, status, logs, and troubleshooting](docs/instances.md)
- [Regions and relays](docs/regions.md) · [location field meanings](docs/locations.md)
- [Bridge access and Zero Trust](docs/access.md) · [NixOS services](docs/nixos.md)
- [Architecture](docs/architecture.md) · [changelog](CHANGELOG.md)

```sh
nix develop --command cargo test --locked
nix flake check
```

The flake checks formatting, Clippy, package tests, the installer against a fixture release, and an isolated NixOS VM using a WARP stand-in. They do not validate live WARP or relay availability.

[MIT](LICENSE). Independent project; not affiliated with Cloudflare or Mudfish.
