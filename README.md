# Waywarp

Waywarp does two things with Cloudflare WARP.

It runs WARP clients side by side. WARP normally runs one client per machine and takes over the system's routing. Waywarp gives each client its own registration, daemon, and network namespace, and exposes it as a local proxy or a network link. Clients never take over your routing or disturb a WARP client you already run.

It also turns WARP into a cross-regional VPN. WARP always connects you near where you are. Waywarp connects each client through a relay in the region you choose, then moves the connection back to your own network. The client keeps the location it got from the relay, and your traffic no longer passes through the relay.

## Requirements

Waywarp runs on x86_64 and aarch64 Linux. It needs these tools on `PATH`:

| Tool | Package |
| --- | --- |
| `warp-svc`, `warp-cli` | [Cloudflare WARP](https://developers.cloudflare.com/warp-client/get-started/linux/) |
| `ip` | iproute2 |
| `nft` | nftables |

Waywarp starts its own daemon for each instance, so the WARP system service can stay disabled.

The kernel must provide `/dev/net/tun`. Proxy access as a normal user needs unprivileged user namespaces; if your distribution restricts them, run it as root instead. Bridge access needs root.

## Install

With Rust 1.89 or newer:

```sh
cargo install --locked --git https://github.com/apersomany/waywarp
```

### Nix

The flake's package includes the runtime tools on Waywarp's `PATH` (using headless WARP, without the tray app). Install it with `nix profile install github:apersomany/waywarp` or add `waywarp.packages.${system}.default` to `environment.systemPackages`. For a one-off development shell, run `nix develop`.

To manage root-owned instances declaratively on NixOS, add the flake as an input and import its module (pass `inputs` through `specialArgs` in your `nixosSystem`):

```nix
# In your flake inputs: waywarp.url = "github:apersomany/waywarp";
{ inputs, ... }: {
  imports = [ inputs.waywarp.nixosModules.default ];
  services.waywarp.instances = {
    home = { index = 0; access.proxy = { }; };
    tokyo = {
      index = 2;
      access.bridge = { };
      location = "geo4=JP";
      via = [ "mudfish:city=tokyo" ];
      environmentFile = "/run/secrets/waywarp-mudfish";
    };
  };
}
```

The example creates `waywarp-home.service` and `waywarp-tokyo.service`, and each attribute name becomes the instance name, so `sudo waywarp status tokyo` works; the module also installs `waywarp`. `access` takes exactly one of `proxy` or `bridge`, holding that access's options, such as `access.proxy.listen` or `access.bridge.subnet4`; the other options mirror `waywarp up`. Units report ready once WARP is connected at the required locations, so other units can order themselves after them, and their logs go to the journal. A unit whose options are invalid fails without restarting.

The environment file, when used, should contain `WAYWARP_MUDFISH_USERNAME=...` and `WAYWARP_MUDFISH_PASSWORD=...`; provide it at runtime through your secret manager, **not** through a Nix store file. The module does not start the host's `cloudflare-warp` service. The flake allows the unfree headless WARP package for its own build; a separately configured WARP package still needs your NixOS unfree policy.

## Getting started

Start a proxy through WARP, send a request through it, and stop it:

```sh
waywarp up proxy
curl -x socks5h://127.0.0.1:1080 https://www.cloudflare.com/cdn-cgi/trace
waywarp down
```

`up` waits until WARP is connected, then prints where the instance landed:

```text
0: connected, proxy 127.0.0.1:1080, geo4 KR/Seoul, geo6 KR/Seoul, edge ICN, probe4 ICN/104.28.211.30, probe6 ICN/2a09:bac1:3f40::20b:a6
```

The first run registers a WARP device, which takes a few seconds longer. Later runs reuse the registration.

## Instances

Every instance has an index from 0 to 255, which commands default to 0. The index keeps instances apart: it picks the proxy port, the bridge link name, the bridge subnets, and which saved registration to use.

An instance can also have a name. `--name` assigns it, replacing any earlier name, and from then on every command accepts the name in place of the index, whether or not the instance is running. Names are unique, up to 32 lowercase letters, digits, and hyphens, and start with a letter.

```sh
waywarp up proxy 1 --name home
waywarp up proxy 2 --name tokyo --location geo4=JP --via mudfish:city=tokyo
waywarp status
waywarp down tokyo
waywarp up proxy tokyo --location geo4=JP --via mudfish:city=tokyo
```

`status` lists running instances; `status --json` prints one JSON object per instance. It exits with status 1 if any of them is not connected or no longer matches its required locations, so it also works as a health check. `waywarp warp-cli INSTANCE ...` runs any `warp-cli` command against one instance's daemon.

`up --foreground` keeps the instance in the foreground for service managers: it logs to stderr, tells systemd when it is ready if `NOTIFY_SOCKET` is set, and stops the instance on SIGTERM.

Root and each user have their own set of instances. Use the same privileges for `status`, `warp-cli`, and `down` as you did for `up`.

## Zero Trust

An existing Cloudflare Zero Trust registration can be copied into a root-owned instance without logging in again:

```sh
sudo waywarp import 3
warp-cli --accept-tos disconnect
sudo waywarp up bridge 3
warp-cli --accept-tos connect
```

The import reads `/var/lib/cloudflare-warp` by default and leaves it unchanged. Use `--from` for another state directory and `--replace` to overwrite an instance registration. Waywarp recognizes Team registrations and selects their WARP edge automatically.

Disconnect the original client while the copied identity makes its first connection. It can be reconnected after `up` succeeds. Both connections can then remain active, although an organization's policy may behave differently. Waywarp refreshes its own copy of the state and never writes it back to the original directory.

Zero Trust policy controls the available access. If it prohibits mode switching, as many include-only policies do, use bridge access. Public geo and probe fields describe direct traffic under such a policy rather than the private routes carried by WARP.

Waywarp does not add a separate enrollment flow. Enroll with the Cloudflare client first, then import its state.

## Access

Every `up` names how the instance is reached.

`up proxy` listens on `127.0.0.1:1080+INDEX` for SOCKS5 and HTTP CONNECT. Point an application at it and its traffic leaves through WARP. `--listen` picks another address, which must be on loopback.

The proxy has no authentication, so any local user can use it. That includes root instances, and with them any Zero Trust identity imported into one; on shared machines, prefer bridge access for those.

`up bridge` needs root. It creates a dual-stack host link called `waywarpINDEX` with IPv4 `/30` and IPv6 `/126` point-to-point subnets. Anything you route to a gateway goes through WARP:

```sh
sudo waywarp up bridge
sudo ip route add 203.0.113.0/24 via 169.254.1.1 dev waywarp0
sudo ip -6 route add 2001:db8:1234::/48 via fd77:6179:7761:7270::1 dev waywarp0
```

Only the link subnets are added automatically, so nothing else goes through WARP until you add routes. `--subnet4` and `--subnet6` replace the derived `/30` and `/126` if either overlaps one of your networks.

The private namespace permits both directions between the host link and WARP. Outbound traffic is translated to WARP's addresses. Unsolicited traffic for those addresses is translated to the host-side link addresses, while traffic routed through WARP keeps its destination. Host routing and host nftables decide whether to accept or forward it; Waywarp never adds host firewall rules.

## Regions

To exit in another region, tell Waywarp where you want to be with `--location` and how to get there with `--via`:

```sh
waywarp up proxy --location geo4=JP --via mudfish:city=tokyo
```

`--location` names status fields and the values they must have, joined with `+`. Case does not matter:

| Field | Takes | Checks |
| --- | --- | --- |
| `geo4`, `geo6` | A country, or `COUNTRY/City` | Where Cloudflare says the WARP IPv4 or IPv6 address is |
| `edge` | A colo | Where the tunnel ends |
| `probe4`, `probe6` | A colo | Which colo served a test request over IPv4 or IPv6 |

Countries are two-letter codes such as `JP`. Colos are Cloudflare's three-letter location codes, which are airport codes such as `NRT`. `geo4=JP` is usually what you want, because it is what most websites that use IP geolocation will see; cities come straight from Cloudflare's geofeed, which spells Tokyo addresses as Tokyo, Narita, or Okubo-naka, so copy them from `status`. [Location fields](docs/locations.md) explains each field and its limits.

`--via` lists ways to reach Cloudflare. Waywarp tries each in order until WARP connects with the required locations, so the relay only has to work once:

| Via | Connects |
| --- | --- |
| `direct` | From your own network, the default |
| `socks5:ADDRESS:PORT` | Through a SOCKS5 relay that supports UDP |
| `mudfish:FILTER` | Through matching Mudfish nodes |

Entries can be mixed, and all but `direct` repeated:

```sh
waywarp up proxy --location geo4=JP+edge=NRT --via socks5:192.0.2.1:1080 --via mudfish:tokyo
```

Once connected, the instance keeps its location. Waywarp checks it again whenever WARP reconnects. If it no longer matches, Waywarp bootstraps again the same way, with a fresh Mudfish node list. With `--no-rebootstrap`, it keeps the connection and marks the mismatch in `status`.

### Mudfish

We recommend [Mudfish](https://mudfish.net) as a relay. It has SOCKS5 nodes with UDP support in many regions and charges by traffic. A bootstrap costs little, because only the connection setup goes through the relay: one measured Tokyo bootstrap sent under 100 KiB, including pings of about thirty nodes. Waywarp is not affiliated with Mudfish in any way.

Waywarp reads the Mudfish node list and picks nodes by name. Plain words match anywhere, and `country`, `region`, `city`, `provider`, and `id` match one field. `+` adds nodes, `-` removes them, and case does not matter:

```sh
mudfish:tokyo+osaka-azure
mudfish:country=jp-provider=aliyun
```

Waywarp pings matching nodes a few at a time and spreads its attempts across providers and cities, so one bad provider does not stall the search.

### Credentials

Relay credentials come from the environment so they stay out of process listings. Waywarp removes them from its environment before starting any helper:

| Variables | For |
| --- | --- |
| `WAYWARP_MUDFISH_USERNAME`, `WAYWARP_MUDFISH_PASSWORD` | `mudfish:` vias |
| `WAYWARP_SOCKS5_USERNAME`, `WAYWARP_SOCKS5_PASSWORD` | `socks5:` vias that need a login |

## Networks

Unless `--interface` pins one, Waywarp sends each new flow out of whichever physical interface the host routes it through, so moving between networks is followed as soon as WARP reconnects. Host VPNs, including WARP itself, are bypassed either way.

## Troubleshooting

Each instance keeps a log of its latest detached run; foreground instances log to stderr instead, which systemd sends to the journal:

- As a user: `~/.local/state/waywarp/INDEX/waywarp.log`, or under `$XDG_STATE_HOME` if set
- As root: `/var/lib/waywarp/INDEX/waywarp.log`

`WAYWARP_LOG` raises the detail for both the command and the instance it starts. It takes a level, optionally followed by per-module levels:

```sh
WAYWARP_LOG=debug waywarp up proxy
WAYWARP_LOG=info,waywarp::dataplane=trace waywarp up proxy
```

## Development

```sh
cargo test
nix flake check
```

`nix develop` provides Rust and the runtime tools. `nix flake check` also runs a NixOS VM test of the module, with a stand-in for WARP. [Architecture](docs/architecture.md) describes how Waywarp works inside.

## License

[MIT](LICENSE). Waywarp is an independent project, not affiliated with Cloudflare or Mudfish.
